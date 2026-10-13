//! P1 schema (DESIGN 3.7) after the gear migrations run on an in-memory
//! SQLite database: tables, documented columns and NOT NULL constraints,
//! key unique constraints / partial unique indexes, foreign keys with
//! cascades, simple value CHECKs, documented defaults, the shared outbox
//! tables, and the `DbErr` -> `DomainError` classification of real driver
//! errors.
//!
//! UUID columns are declared `TEXT` on SQLite but carry 16-byte blobs
//! (DESIGN 3.7): the helpers below write `x'<32 hex>'` literals.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use std::collections::{BTreeMap, BTreeSet};

use mini_chat::domain::error::DomainError;
use mini_chat::infra::db::error::classify_db_err;
use mini_chat::infra::db::migrations::{Migrator, gear_migrations};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, Statement};
use sea_orm_migration::{MigratorTrait, SchemaManager};

// ── harness ──────────────────────────────────────────────────────────────────

fn stmt(db: &DatabaseConnection, sql: impl Into<String>) -> Statement {
    Statement::from_string(db.get_database_backend(), sql.into())
}

async fn exec(db: &DatabaseConnection, sql: impl Into<String>) -> Result<(), DbErr> {
    db.execute_raw(stmt(db, sql)).await.map(|_| ())
}

/// Fresh in-memory SQLite with foreign keys enforced and every gear migration
/// (domain schema + outbox) applied, exactly as `DatabaseCapability::migrations()`
/// would run them.
async fn migrated_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    exec(&db, "PRAGMA foreign_keys = ON;").await.unwrap();
    let manager = SchemaManager::new(&db);
    for migration in gear_migrations() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("migration {} failed: {e}", migration.name()));
    }
    db
}

async fn table_names(db: &DatabaseConnection) -> BTreeSet<String> {
    let rows = db
        .query_all_raw(stmt(
            db,
            "SELECT name FROM sqlite_master WHERE type = 'table'",
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| r.try_get::<String>("", "name").unwrap())
        .collect()
}

/// column name -> NOT NULL flag
async fn columns(db: &DatabaseConnection, table: &str) -> BTreeMap<String, bool> {
    let rows = db
        .query_all_raw(stmt(db, format!("PRAGMA table_info({table})")))
        .await
        .unwrap();
    rows.iter()
        .map(|r| {
            (
                r.try_get::<String>("", "name").unwrap(),
                r.try_get::<i64>("", "notnull").unwrap() != 0,
            )
        })
        .collect()
}

/// `CREATE INDEX` statements of a table (`sqlite_master.sql`), lowercased.
async fn index_sql(db: &DatabaseConnection, table: &str) -> Vec<String> {
    let rows = db
        .query_all_raw(stmt(
            db,
            format!(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = '{table}' AND sql IS NOT NULL"
            ),
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| r.try_get::<String>("", "sql").unwrap().to_lowercase())
        .collect()
}

async fn index_names(db: &DatabaseConnection) -> BTreeSet<String> {
    let rows = db
        .query_all_raw(stmt(
            db,
            "SELECT name FROM sqlite_master WHERE type = 'index'",
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| r.try_get::<String>("", "name").unwrap())
        .collect()
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(stmt(db, sql))
        .await
        .unwrap()
        .expect("one row")
        .try_get::<i64>("", "n")
        .unwrap()
}

async fn scalar_string(db: &DatabaseConnection, sql: &str) -> String {
    db.query_one_raw(stmt(db, sql))
        .await
        .unwrap()
        .expect("one row")
        .try_get::<String>("", "v")
        .unwrap()
}

/// 16-byte blob literal of a small number.
fn id(n: u128) -> String {
    format!("x'{n:032x}'")
}

const TS: &str = "'2026-03-02T10:00:00+00:00'";
const TENANT: u128 = 0xA0;
const USER: u128 = 0xB0;

async fn insert_chat(db: &DatabaseConnection, chat: u128) -> Result<(), DbErr> {
    exec(
        db,
        format!(
            "INSERT INTO chats (id, tenant_id, user_id, model, title, is_temporary, created_at, updated_at) \
             VALUES ({}, {}, {}, 'gpt-4.1', 'T', 0, {TS}, {TS})",
            id(chat),
            id(TENANT),
            id(USER)
        ),
    )
    .await
}

async fn insert_message(
    db: &DatabaseConnection,
    msg: u128,
    chat: u128,
    request: Option<u128>,
    role: &str,
    deleted: bool,
) -> Result<(), DbErr> {
    let request = request.map_or_else(|| "NULL".to_owned(), id);
    let deleted_at = if deleted { TS } else { "NULL" };
    exec(
        db,
        format!(
            "INSERT INTO messages (id, tenant_id, chat_id, request_id, role, content, content_type, \
             token_estimate, request_kind, features_used, input_tokens, output_tokens, is_compressed, \
             created_at, deleted_at) \
             VALUES ({}, {}, {}, {request}, '{role}', 'hello', 'text', 0, 'chat', '[]', 0, 0, 0, {TS}, {deleted_at})",
            id(msg),
            id(TENANT),
            id(chat)
        ),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn insert_turn(
    db: &DatabaseConnection,
    turn: u128,
    chat: u128,
    request: u128,
    state: &str,
    requester_type: &str,
    deleted: bool,
) -> Result<(), DbErr> {
    let completed_at = if state == "running" { "NULL" } else { TS };
    let deleted_at = if deleted { TS } else { "NULL" };
    exec(
        db,
        format!(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, requester_user_id, \
             state, started_at, last_progress_at, completed_at, deleted_at, updated_at) \
             VALUES ({}, {}, {}, {}, '{requester_type}', {}, '{state}', {TS}, {TS}, {completed_at}, {deleted_at}, {TS})",
            id(turn),
            id(TENANT),
            id(chat),
            id(request),
            id(USER)
        ),
    )
    .await
}

async fn insert_attachment(
    db: &DatabaseConnection,
    att: u128,
    chat: u128,
    status: &str,
    kind: &str,
) -> Result<(), DbErr> {
    exec(
        db,
        format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, content_type, \
             size_bytes, storage_backend, status, attachment_kind, created_at, updated_at) \
             VALUES ({}, {}, {}, {}, 'a.pdf', 'application/pdf', 10, 'openai', '{status}', '{kind}', {TS}, {TS})",
            id(att),
            id(TENANT),
            id(chat),
            id(USER)
        ),
    )
    .await
}

async fn insert_quota(
    db: &DatabaseConnection,
    row: u128,
    period_type: &str,
    period_start: &str,
    bucket: &str,
) -> Result<(), DbErr> {
    exec(
        db,
        format!(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, \
             spent_credits_micro, reserved_credits_micro, calls, input_tokens, output_tokens, \
             file_search_calls, web_search_calls, code_interpreter_calls, rag_retrieval_calls, \
             image_inputs, image_upload_bytes, updated_at) \
             VALUES ({}, {}, {}, '{period_type}', '{period_start}', '{bucket}', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, {TS})",
            id(row),
            id(TENANT),
            id(USER)
        ),
    )
    .await
}

async fn insert_reaction(
    db: &DatabaseConnection,
    row: u128,
    message: u128,
    user: u128,
    reaction: &str,
) -> Result<(), DbErr> {
    exec(
        db,
        format!(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, created_at) \
             VALUES ({}, {}, {}, {}, '{reaction}', {TS})",
            id(row),
            id(message),
            id(user),
            id(TENANT)
        ),
    )
    .await
}

fn is_constraint_failure(r: Result<(), DbErr>) -> bool {
    r.is_err()
}

// ── tables ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn all_p1_tables_exist() {
    let db = migrated_db().await;
    let tables = table_names(&db).await;
    for t in [
        "chats",
        "messages",
        "chat_turns",
        "attachments",
        "message_attachments",
        "thread_summaries",
        "chat_vector_stores",
        "quota_usage",
        "message_reactions",
    ] {
        assert!(tables.contains(t), "table `{t}` missing; have {tables:?}");
    }
}

#[tokio::test]
async fn mcp_and_projection_tables_are_not_created() {
    let db = migrated_db().await;
    let tables = table_names(&db).await;
    for t in [
        "mcp_servers",
        "mcp_server_tools",
        "role_mcp_servers",
        "tenant_closure",
    ] {
        assert!(!tables.contains(t), "`{t}` must not exist (ADR-0006, 3.7)");
    }
    assert!(
        !tables.iter().any(|t| t.starts_with("mcp")),
        "no mcp_* table: {tables:?}"
    );
}

#[tokio::test]
async fn outbox_tables_are_created_by_the_gear_migrations() {
    let db = migrated_db().await;
    let tables = table_names(&db).await;
    let outbox: Vec<&String> = tables.iter().filter(|t| t.contains("outbox")).collect();
    assert!(
        outbox.iter().any(|t| t.contains("incoming")),
        "outbox incoming table missing: {outbox:?}"
    );
    assert!(
        outbox.iter().any(|t| t.contains("outgoing")),
        "outbox outgoing table missing: {outbox:?}"
    );
}

#[tokio::test]
async fn domain_migrator_alone_creates_the_domain_schema_without_outbox() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    exec(&db, "PRAGMA foreign_keys = ON;").await.unwrap();
    Migrator::up(&db, None)
        .await
        .expect("domain migrations apply");
    let tables = table_names(&db).await;
    assert!(tables.contains("chats"));
    assert!(tables.contains("quota_usage"));
    assert!(
        !tables.iter().any(|t| t.contains("outbox")),
        "outbox DDL comes from the toolkit outbox migrations, not from `Migrator`"
    );
}

#[tokio::test]
async fn migration_names_are_unique() {
    let names: Vec<String> = gear_migrations()
        .iter()
        .map(|m| m.name().to_owned())
        .collect();
    let unique: BTreeSet<&String> = names.iter().collect();
    assert_eq!(
        names.len(),
        unique.len(),
        "duplicate migration names: {names:?}"
    );
    assert!(!names.is_empty());
}

// ── columns ──────────────────────────────────────────────────────────────────

async fn assert_columns(
    db: &DatabaseConnection,
    table: &str,
    expected: &[&str],
    not_null: &[&str],
) {
    let cols = columns(db, table).await;
    for c in expected {
        assert!(
            cols.contains_key(*c),
            "{table}.{c} missing; have {:?}",
            cols.keys()
        );
    }
    for c in not_null {
        assert_eq!(cols.get(*c), Some(&true), "{table}.{c} must be NOT NULL");
    }
}

#[tokio::test]
async fn chats_columns() {
    let db = migrated_db().await;
    assert_columns(
        &db,
        "chats",
        &[
            "id",
            "tenant_id",
            "user_id",
            "model",
            "title",
            "is_temporary",
            "created_at",
            "updated_at",
            "deleted_at",
        ],
        &["tenant_id", "user_id", "created_at", "updated_at"],
    )
    .await;
    let cols = columns(&db, "chats").await;
    assert!(!cols["deleted_at"], "soft delete timestamp is nullable");
}

#[tokio::test]
async fn messages_columns() {
    let db = migrated_db().await;
    assert_columns(
        &db,
        "messages",
        &[
            "id",
            "tenant_id",
            "chat_id",
            "request_id",
            "role",
            "content",
            "content_type",
            "token_estimate",
            "provider_response_id",
            "request_kind",
            "features_used",
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_write_input_tokens",
            "reasoning_tokens",
            "model",
            "is_compressed",
            "created_at",
            "deleted_at",
        ],
        &[
            "tenant_id",
            "chat_id",
            "role",
            "content",
            "content_type",
            "created_at",
        ],
    )
    .await;
    let cols = columns(&db, "messages").await;
    assert!(!cols["request_id"], "request_id is nullable");
    assert!(!cols["model"], "effective model is nullable");
}

#[tokio::test]
async fn chat_turns_columns() {
    let db = migrated_db().await;
    assert_columns(
        &db,
        "chat_turns",
        &[
            "id",
            "tenant_id",
            "chat_id",
            "request_id",
            "requester_type",
            "requester_user_id",
            "state",
            "provider_name",
            "provider_response_id",
            "assistant_message_id",
            "error_code",
            "reserve_tokens",
            "max_output_tokens_applied",
            "reserved_credits_micro",
            "policy_version_applied",
            "effective_model",
            "minimal_generation_floor_applied",
            "error_detail",
            "deleted_at",
            "replaced_by_request_id",
            "started_at",
            "last_progress_at",
            "web_search_enabled",
            "web_search_completed_count",
            "code_interpreter_completed_count",
            "file_search_completed_count",
            "completed_at",
            "updated_at",
        ],
        &[
            "tenant_id",
            "chat_id",
            "request_id",
            "requester_type",
            "state",
            "started_at",
        ],
    )
    .await;
    let cols = columns(&db, "chat_turns").await;
    for nullable in [
        "reserve_tokens",
        "max_output_tokens_applied",
        "reserved_credits_micro",
        "policy_version_applied",
        "effective_model",
        "minimal_generation_floor_applied",
        "last_progress_at",
        "completed_at",
        "requester_user_id",
    ] {
        assert!(
            !cols[nullable],
            "chat_turns.{nullable} is nullable (pre-reserve failures, system turns)"
        );
    }
}

#[tokio::test]
async fn attachments_columns() {
    let db = migrated_db().await;
    assert_columns(
        &db,
        "attachments",
        &[
            "id",
            "tenant_id",
            "chat_id",
            "uploaded_by_user_id",
            "filename",
            "content_type",
            "size_bytes",
            "storage_backend",
            "provider_file_id",
            "status",
            "error_code",
            "attachment_kind",
            "for_file_search",
            "for_code_interpreter",
            "doc_summary",
            "img_thumbnail",
            "img_thumbnail_width",
            "img_thumbnail_height",
            "summary_model",
            "summary_updated_at",
            "cleanup_status",
            "cleanup_attempts",
            "last_cleanup_error",
            "cleanup_updated_at",
            "created_at",
            "updated_at",
            "deleted_at",
            "secondary_file_id",
            "secondary_status",
            "secondary_provider_kind",
        ],
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
    )
    .await;
}

#[tokio::test]
async fn message_attachments_columns() {
    let db = migrated_db().await;
    assert_columns(
        &db,
        "message_attachments",
        &[
            "tenant_id",
            "chat_id",
            "message_id",
            "attachment_id",
            "created_at",
        ],
        &["tenant_id", "chat_id", "created_at"],
    )
    .await;
}

#[tokio::test]
async fn thread_summaries_columns() {
    let db = migrated_db().await;
    assert_columns(
        &db,
        "thread_summaries",
        &[
            "id",
            "tenant_id",
            "chat_id",
            "summary_text",
            "summarized_up_to_created_at",
            "summarized_up_to_message_id",
            "token_estimate",
            "created_at",
            "updated_at",
        ],
        &[
            "tenant_id",
            "summarized_up_to_created_at",
            "summarized_up_to_message_id",
            "created_at",
            "updated_at",
        ],
    )
    .await;
}

#[tokio::test]
async fn chat_vector_stores_columns() {
    let db = migrated_db().await;
    assert_columns(
        &db,
        "chat_vector_stores",
        &[
            "id",
            "tenant_id",
            "chat_id",
            "vector_store_id",
            "provider",
            "file_count",
            "created_at",
        ],
        &["provider", "created_at"],
    )
    .await;
    let cols = columns(&db, "chat_vector_stores").await;
    assert!(
        !cols["vector_store_id"],
        "vector_store_id is NULL while provider creation is in progress"
    );
}

#[tokio::test]
async fn quota_usage_columns() {
    let db = migrated_db().await;
    assert_columns(
        &db,
        "quota_usage",
        &[
            "id",
            "tenant_id",
            "user_id",
            "period_type",
            "period_start",
            "bucket",
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
            "updated_at",
        ],
        &[
            "tenant_id",
            "user_id",
            "period_type",
            "period_start",
            "bucket",
        ],
    )
    .await;
}

#[tokio::test]
async fn message_reactions_columns() {
    let db = migrated_db().await;
    assert_columns(
        &db,
        "message_reactions",
        &[
            "id",
            "message_id",
            "user_id",
            "tenant_id",
            "reaction",
            "created_at",
        ],
        &[
            "message_id",
            "user_id",
            "tenant_id",
            "reaction",
            "created_at",
        ],
    )
    .await;
}

#[tokio::test]
async fn every_table_has_a_tenant_id_column() {
    // 3.7 "Tenant scoping": every table has `tenant_id` (the Secure ORM tenant column).
    let db = migrated_db().await;
    for t in [
        "chats",
        "messages",
        "chat_turns",
        "attachments",
        "message_attachments",
        "thread_summaries",
        "chat_vector_stores",
        "quota_usage",
        "message_reactions",
    ] {
        assert!(
            columns(&db, t).await.contains_key("tenant_id"),
            "{t}.tenant_id"
        );
    }
}

// ── unique constraints / partial unique indexes ──────────────────────────────

#[tokio::test]
async fn chat_turns_request_id_is_unique_per_chat() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_chat(&db, 2).await.unwrap();
    insert_turn(&db, 10, 1, 100, "completed", "user", false)
        .await
        .unwrap();

    assert!(
        is_constraint_failure(insert_turn(&db, 11, 1, 100, "completed", "user", false).await),
        "UNIQUE(chat_id, request_id)"
    );
    // the same request id in another chat is fine
    insert_turn(&db, 12, 2, 100, "completed", "user", false)
        .await
        .unwrap();
}

#[tokio::test]
async fn only_one_running_turn_per_chat() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_chat(&db, 2).await.unwrap();

    insert_turn(&db, 10, 1, 100, "running", "user", false)
        .await
        .unwrap();
    assert!(
        is_constraint_failure(insert_turn(&db, 11, 1, 101, "running", "user", false).await),
        "UNIQUE(chat_id) WHERE state = 'running' AND deleted_at IS NULL"
    );
    // other chats are independent
    insert_turn(&db, 12, 2, 101, "running", "user", false)
        .await
        .unwrap();
    // terminal turns do not count
    insert_turn(&db, 13, 1, 102, "completed", "user", false)
        .await
        .unwrap();
    insert_turn(&db, 14, 1, 103, "failed", "user", false)
        .await
        .unwrap();
    insert_turn(&db, 15, 1, 104, "cancelled", "user", false)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_new_running_turn_is_allowed_once_the_previous_one_is_terminal_or_deleted() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_turn(&db, 10, 1, 100, "running", "user", false)
        .await
        .unwrap();

    exec(
        &db,
        format!(
            "UPDATE chat_turns SET state = 'completed', completed_at = {TS} WHERE id = {}",
            id(10)
        ),
    )
    .await
    .unwrap();
    insert_turn(&db, 11, 1, 101, "running", "user", false)
        .await
        .unwrap();

    // soft-deleted running rows are excluded from the guard
    exec(
        &db,
        format!(
            "UPDATE chat_turns SET deleted_at = {TS} WHERE id = {}",
            id(11)
        ),
    )
    .await
    .unwrap();
    insert_turn(&db, 12, 1, 102, "running", "user", false)
        .await
        .unwrap();
}

#[tokio::test]
async fn message_request_id_is_unique_per_chat_and_role_among_live_rows() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();

    insert_message(&db, 10, 1, Some(100), "user", false)
        .await
        .unwrap();
    // one user and one assistant message per request id
    insert_message(&db, 11, 1, Some(100), "assistant", false)
        .await
        .unwrap();
    // a second user message for the same request id is rejected
    assert!(
        is_constraint_failure(insert_message(&db, 12, 1, Some(100), "user", false).await),
        "UNIQUE(chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL"
    );
    // soft-deleted rows are exempt
    insert_message(&db, 13, 1, Some(100), "user", true)
        .await
        .unwrap();
    // rows without a request id are exempt
    insert_message(&db, 14, 1, None, "user", false)
        .await
        .unwrap();
    insert_message(&db, 15, 1, None, "user", false)
        .await
        .unwrap();
}

#[tokio::test]
async fn quota_usage_bucket_row_is_unique() {
    let db = migrated_db().await;
    insert_quota(&db, 1, "daily", "2026-03-02", "total")
        .await
        .unwrap();

    assert!(
        is_constraint_failure(insert_quota(&db, 2, "daily", "2026-03-02", "total").await),
        "UNIQUE(tenant_id, user_id, period_type, period_start, bucket)"
    );
    // each coordinate of the key distinguishes rows
    insert_quota(&db, 3, "daily", "2026-03-02", "tier:premium")
        .await
        .unwrap();
    insert_quota(&db, 4, "monthly", "2026-03-02", "total")
        .await
        .unwrap();
    insert_quota(&db, 5, "daily", "2026-03-03", "total")
        .await
        .unwrap();
}

#[tokio::test]
async fn one_reaction_per_user_per_message() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_message(&db, 10, 1, Some(100), "assistant", false)
        .await
        .unwrap();

    insert_reaction(&db, 1, 10, USER, "like").await.unwrap();
    assert!(
        is_constraint_failure(insert_reaction(&db, 2, 10, USER, "dislike").await),
        "UNIQUE(message_id, user_id)"
    );
    insert_reaction(&db, 3, 10, USER + 1, "dislike")
        .await
        .unwrap();
}

#[tokio::test]
async fn one_thread_summary_per_chat() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    let insert = |row: u128| {
        exec(
            &db,
            format!(
                "INSERT INTO thread_summaries (id, tenant_id, chat_id, summary_text, \
                 summarized_up_to_created_at, summarized_up_to_message_id, token_estimate, \
                 created_at, updated_at) \
                 VALUES ({}, {}, {}, 'summary', {TS}, {}, 5, {TS}, {TS})",
                id(row),
                id(TENANT),
                id(1),
                id(99)
            ),
        )
    };
    insert(1).await.unwrap();
    assert!(is_constraint_failure(insert(2).await), "UNIQUE(chat_id)");
}

#[tokio::test]
async fn one_vector_store_row_per_tenant_and_chat() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    let insert = |row: u128, tenant: u128, store: &str| {
        exec(
            &db,
            format!(
                "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, \
                 file_count, created_at) VALUES ({}, {}, {}, {store}, 'openai', 0, {TS})",
                id(row),
                id(tenant),
                id(1)
            ),
        )
    };
    // placeholder row: vector_store_id NULL while creation is in progress
    insert(1, TENANT, "NULL").await.unwrap();
    assert!(
        is_constraint_failure(insert(2, TENANT, "'vs_1'").await),
        "UNIQUE(tenant_id, chat_id)"
    );
    insert(3, TENANT + 1, "'vs_2'").await.unwrap();
}

// ── foreign keys ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn deleting_a_chat_cascades_to_children() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_message(&db, 10, 1, Some(100), "user", false)
        .await
        .unwrap();
    insert_turn(&db, 20, 1, 100, "completed", "user", false)
        .await
        .unwrap();
    insert_attachment(&db, 30, 1, "ready", "document")
        .await
        .unwrap();

    exec(&db, format!("DELETE FROM chats WHERE id = {}", id(1)))
        .await
        .unwrap();

    for t in ["messages", "chat_turns", "attachments"] {
        assert_eq!(
            scalar_i64(&db, &format!("SELECT COUNT(*) AS n FROM {t}")).await,
            0,
            "{t} rows must cascade with their chat"
        );
    }
}

#[tokio::test]
async fn children_require_an_existing_chat() {
    let db = migrated_db().await;
    assert!(is_constraint_failure(
        insert_message(&db, 10, 404, Some(100), "user", false).await
    ));
    assert!(is_constraint_failure(
        insert_turn(&db, 20, 404, 100, "running", "user", false).await
    ));
    assert!(is_constraint_failure(
        insert_attachment(&db, 30, 404, "ready", "document").await
    ));
}

#[tokio::test]
async fn message_attachments_enforce_that_both_rows_belong_to_the_same_chat() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_chat(&db, 2).await.unwrap();
    insert_message(&db, 10, 1, Some(100), "user", false)
        .await
        .unwrap();
    insert_attachment(&db, 30, 1, "ready", "document")
        .await
        .unwrap();
    insert_attachment(&db, 31, 2, "ready", "document")
        .await
        .unwrap();

    let link = |chat: u128, message: u128, attachment: u128| {
        exec(
            &db,
            format!(
                "INSERT INTO message_attachments (tenant_id, chat_id, message_id, attachment_id, created_at) \
                 VALUES ({}, {}, {}, {}, {TS})",
                id(TENANT),
                id(chat),
                id(message),
                id(attachment)
            ),
        )
    };

    link(1, 10, 30).await.unwrap();
    assert!(
        is_constraint_failure(link(1, 10, 31).await),
        "attachment of another chat: composite FK (attachment_id, chat_id)"
    );
    assert!(
        is_constraint_failure(link(2, 10, 31).await),
        "message of another chat: composite FK (message_id, chat_id)"
    );
    // the primary key is (chat_id, message_id, attachment_id)
    assert!(
        is_constraint_failure(link(1, 10, 30).await),
        "duplicate link"
    );
}

#[tokio::test]
async fn message_attachment_links_cascade_with_the_message_and_the_attachment() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_message(&db, 10, 1, Some(100), "user", false)
        .await
        .unwrap();
    insert_message(&db, 11, 1, Some(101), "user", false)
        .await
        .unwrap();
    insert_attachment(&db, 30, 1, "ready", "document")
        .await
        .unwrap();
    for message in [10_u128, 11] {
        exec(
            &db,
            format!(
                "INSERT INTO message_attachments (tenant_id, chat_id, message_id, attachment_id, created_at) \
                 VALUES ({}, {}, {}, {}, {TS})",
                id(TENANT),
                id(1),
                id(message),
                id(30)
            ),
        )
        .await
        .unwrap();
    }

    exec(&db, format!("DELETE FROM messages WHERE id = {}", id(10)))
        .await
        .unwrap();
    assert_eq!(
        scalar_i64(&db, "SELECT COUNT(*) AS n FROM message_attachments").await,
        1
    );
    exec(
        &db,
        format!("DELETE FROM attachments WHERE id = {}", id(30)),
    )
    .await
    .unwrap();
    assert_eq!(
        scalar_i64(&db, "SELECT COUNT(*) AS n FROM message_attachments").await,
        0
    );
}

#[tokio::test]
async fn reactions_cascade_with_their_message_and_need_one() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_message(&db, 10, 1, Some(100), "assistant", false)
        .await
        .unwrap();

    assert!(
        is_constraint_failure(insert_reaction(&db, 1, 404, USER, "like").await),
        "FK message_id -> messages(id)"
    );
    insert_reaction(&db, 2, 10, USER, "like").await.unwrap();
    exec(&db, format!("DELETE FROM messages WHERE id = {}", id(10)))
        .await
        .unwrap();
    assert_eq!(
        scalar_i64(&db, "SELECT COUNT(*) AS n FROM message_reactions").await,
        0
    );
}

#[tokio::test]
async fn thread_summary_cascades_with_its_chat() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    exec(
        &db,
        format!(
            "INSERT INTO thread_summaries (id, tenant_id, chat_id, summary_text, \
             summarized_up_to_created_at, summarized_up_to_message_id, token_estimate, created_at, updated_at) \
             VALUES ({}, {}, {}, 's', {TS}, {}, 1, {TS}, {TS})",
            id(1),
            id(TENANT),
            id(1),
            id(2)
        ),
    )
    .await
    .unwrap();
    exec(&db, format!("DELETE FROM chats WHERE id = {}", id(1)))
        .await
        .unwrap();
    assert_eq!(
        scalar_i64(&db, "SELECT COUNT(*) AS n FROM thread_summaries").await,
        0
    );
}

// ── simple value CHECKs ──────────────────────────────────────────────────────

#[tokio::test]
async fn chat_turn_state_and_requester_type_are_checked() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();

    for (i, state) in ["running", "completed", "failed", "cancelled"]
        .iter()
        .enumerate()
    {
        // distinct chats avoid tripping the one-running-turn index
        let chat = 100 + i as u128;
        insert_chat(&db, chat).await.unwrap();
        insert_turn(&db, 200 + i as u128, chat, 1, state, "user", false)
            .await
            .unwrap_or_else(|e| panic!("state `{state}` must be accepted: {e}"));
    }
    assert!(is_constraint_failure(
        insert_turn(&db, 300, 1, 2, "paused", "user", false).await
    ));
    assert!(is_constraint_failure(
        insert_turn(&db, 301, 1, 3, "completed", "robot", false).await
    ));
    // `system` is a valid requester type
    insert_turn(&db, 302, 1, 4, "completed", "system", false)
        .await
        .unwrap();
}

#[tokio::test]
async fn attachment_kind_and_status_are_checked() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();

    for (i, status) in ["pending", "uploaded", "ready", "failed"]
        .iter()
        .enumerate()
    {
        insert_attachment(&db, 10 + i as u128, 1, status, "document")
            .await
            .unwrap_or_else(|e| panic!("status `{status}` must be accepted: {e}"));
    }
    insert_attachment(&db, 20, 1, "ready", "image")
        .await
        .unwrap();

    assert!(is_constraint_failure(
        insert_attachment(&db, 21, 1, "ready", "video").await
    ));
    assert!(is_constraint_failure(
        insert_attachment(&db, 22, 1, "deleted", "document").await
    ));
}

#[tokio::test]
async fn reaction_value_is_checked() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_message(&db, 10, 1, Some(100), "assistant", false)
        .await
        .unwrap();

    insert_reaction(&db, 1, 10, USER, "like").await.unwrap();
    insert_reaction(&db, 2, 10, USER + 1, "dislike")
        .await
        .unwrap();
    assert!(is_constraint_failure(
        insert_reaction(&db, 3, 10, USER + 2, "love").await
    ));
}

#[tokio::test]
async fn vector_store_file_count_cannot_be_negative() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    let r = exec(
        &db,
        format!(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, file_count, created_at) \
             VALUES ({}, {}, {}, 'vs', 'openai', -1, {TS})",
            id(1),
            id(TENANT),
            id(1)
        ),
    )
    .await;
    assert!(is_constraint_failure(r), "CHECK file_count >= 0");
}

// ── documented defaults ──────────────────────────────────────────────────────

#[tokio::test]
async fn documented_column_defaults_apply() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();

    // chat_turns: counters default to 0 and `web_search_enabled` to false
    exec(
        &db,
        format!(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, state, started_at, \
             last_progress_at, updated_at) VALUES ({}, {}, {}, {}, 'user', 'running', {TS}, {TS}, {TS})",
            id(1),
            id(TENANT),
            id(1),
            id(100)
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT web_search_enabled + web_search_completed_count + code_interpreter_completed_count \
             + file_search_completed_count AS n FROM chat_turns"
        )
        .await,
        0
    );

    // attachments: storage_backend `azure`, secondary_status `not_attempted`, cleanup_attempts 0
    exec(
        &db,
        format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, status, \
             attachment_kind, created_at) VALUES ({}, {}, {}, {}, 'f', 'pending', 'document', {TS})",
            id(2),
            id(TENANT),
            id(1),
            id(USER)
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        scalar_string(&db, "SELECT storage_backend AS v FROM attachments").await,
        "azure"
    );
    assert_eq!(
        scalar_string(&db, "SELECT secondary_status AS v FROM attachments").await,
        "not_attempted"
    );
    assert_eq!(
        scalar_i64(&db, "SELECT cleanup_attempts AS n FROM attachments").await,
        0
    );

    // quota_usage: spend / reserve / telemetry counters default to 0
    exec(
        &db,
        format!(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, updated_at) \
             VALUES ({}, {}, {}, 'daily', '2026-03-02', 'total', {TS})",
            id(3),
            id(TENANT),
            id(USER)
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT spent_credits_micro + reserved_credits_micro + calls + input_tokens + output_tokens \
             + web_search_calls + code_interpreter_calls + rag_retrieval_calls AS n FROM quota_usage"
        )
        .await,
        0
    );
}

// ── indexes ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn attachment_reaper_scan_index_exists_under_its_documented_name() {
    let db = migrated_db().await;
    assert!(
        index_names(&db)
            .await
            .contains("idx_attachments_stale_upload"),
        "DESIGN 3.7: idx_attachments_stale_upload"
    );
    let sql = index_sql(&db, "attachments").await;
    let stale = sql
        .iter()
        .find(|s| s.contains("idx_attachments_stale_upload"))
        .expect("index definition");
    assert!(
        !stale.contains(" where "),
        "the reaper index is a plain index, not a partial one: {stale}"
    );
    for col in ["status", "cleanup_status", "deleted_at", "updated_at"] {
        assert!(stale.contains(col), "{col} in {stale}");
    }
}

#[tokio::test]
async fn listing_and_scan_indexes_exist() {
    let db = migrated_db().await;

    let chats = index_sql(&db, "chats").await;
    assert!(
        chats.iter().any(|s| s.contains("tenant_id")
            && s.contains("user_id")
            && s.contains("updated_at")
            && s.contains("deleted_at is null")),
        "chats listing index (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL: {chats:?}"
    );

    let messages = index_sql(&db, "messages").await;
    assert!(
        messages.iter().any(|s| s.contains("chat_id")
            && s.contains("created_at")
            && s.contains("deleted_at is null")),
        "messages chronological index: {messages:?}"
    );

    let turns = index_sql(&db, "chat_turns").await;
    assert!(
        turns
            .iter()
            .any(|s| s.contains("last_progress_at") && s.contains("running")),
        "orphan-scan index on last_progress_at WHERE state = 'running': {turns:?}"
    );
    assert!(
        turns.iter().any(|s| s.contains("started_at")),
        "latest-turn index on (chat_id, started_at DESC): {turns:?}"
    );
}

// ── DbErr classification of real driver errors ───────────────────────────────

#[tokio::test]
async fn a_real_unique_violation_is_classified_as_a_unique_conflict() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    let err = insert_chat(&db, 1)
        .await
        .expect_err("duplicate primary key");

    let mapped = classify_db_err(err);
    assert!(
        matches!(
            mapped,
            DomainError::Conflict {
                code: "unique_violation"
            }
        ),
        "got {mapped:?}"
    );
}

#[tokio::test]
async fn a_unique_index_violation_is_classified_as_a_unique_conflict() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    insert_turn(&db, 10, 1, 100, "running", "user", false)
        .await
        .unwrap();
    let err = insert_turn(&db, 11, 1, 101, "running", "user", false)
        .await
        .expect_err("second running turn");
    assert!(matches!(
        classify_db_err(err),
        DomainError::Conflict {
            code: "unique_violation"
        }
    ));
}

#[tokio::test]
async fn a_check_violation_is_an_internal_error() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await.unwrap();
    let err = insert_turn(&db, 10, 1, 100, "paused", "user", false)
        .await
        .expect_err("CHECK on state");
    let mapped = classify_db_err(err);
    assert!(
        matches!(mapped, DomainError::Internal { .. }),
        "a firing CHECK is a broken server-side invariant, not a client error: {mapped:?}"
    );
}

#[tokio::test]
async fn an_unclassified_db_error_is_an_internal_error() {
    let mapped = classify_db_err(DbErr::Custom("connection refused".to_owned()));
    assert!(matches!(mapped, DomainError::Internal { .. }), "{mapped:?}");
}
