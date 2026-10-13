#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::type_complexity,
    clippy::cognitive_complexity
)]

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ConnectionTrait, Database, DatabaseConnection, EntityTrait,
    Statement,
};
use sea_orm_migration::{MigratorTrait, SchemaManager};
use serde_json::json;
use uuid::Uuid;

use super::entities::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment, message_reaction,
    quota_usage, thread_summary,
};
use super::migrations::{Migrator, m0001_initial};
use super::{OUTBOX_TABLE_PREFIX, all_migrations};

const TS: &str = "2026-10-04T00:00:00+00:00";

fn stmt(db: &DatabaseConnection, sql: impl Into<String>) -> Statement {
    Statement::from_string(db.get_database_backend(), sql.into())
}

async fn exec(db: &DatabaseConnection, sql: &str) -> Result<(), sea_orm::DbErr> {
    db.execute_raw(stmt(db, sql)).await.map(|_| ())
}

/// In-memory `SQLite` (single connection) with FK enforcement and the gear migrations.
async fn migrated_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    exec(&db, "PRAGMA foreign_keys = ON;")
        .await
        .expect("fk pragma");
    Migrator::up(&db, None).await.expect("migrate");
    db
}

/// 16-byte blob literal for UUID columns.
fn u(n: u8) -> String {
    format!("x'{:032x}'", u128::from(n))
}

async fn table_names(db: &DatabaseConnection) -> BTreeSet<String> {
    let rows = db
        .query_all_raw(stmt(
            db,
            "SELECT name FROM sqlite_master WHERE type = 'table'",
        ))
        .await
        .expect("tables");
    rows.iter()
        .map(|r| r.try_get::<String>("", "name").expect("name"))
        .collect()
}

async fn columns(db: &DatabaseConnection, table: &str) -> Vec<(String, String, bool)> {
    let rows = db
        .query_all_raw(stmt(db, format!("PRAGMA table_info({table})")))
        .await
        .expect("table_info");
    rows.iter()
        .map(|r| {
            (
                r.try_get::<String>("", "name").expect("name"),
                r.try_get::<String>("", "type").expect("type"),
                r.try_get::<i64>("", "notnull").expect("notnull") != 0,
            )
        })
        .collect()
}

async fn insert_chat(db: &DatabaseConnection, id: u8) {
    exec(
        db,
        &format!(
            "INSERT INTO chats (id, tenant_id, user_id, model, created_at, updated_at) \
             VALUES ({}, {}, {}, 'gpt-x', '{TS}', '{TS}')",
            u(id),
            u(100),
            u(101)
        ),
    )
    .await
    .expect("insert chat");
}

async fn insert_turn(
    db: &DatabaseConnection,
    id: u8,
    chat: u8,
    request: u8,
    state: &str,
    deleted_at: Option<&str>,
) -> Result<(), sea_orm::DbErr> {
    let deleted = deleted_at.map_or_else(|| "NULL".to_owned(), |d| format!("'{d}'"));
    exec(
        db,
        &format!(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, state, \
             started_at, last_progress_at, updated_at, deleted_at) \
             VALUES ({}, {}, {}, {}, 'user', '{state}', '{TS}', '{TS}', '{TS}', {deleted})",
            u(id),
            u(100),
            u(chat),
            u(request)
        ),
    )
    .await
}

async fn insert_message(
    db: &DatabaseConnection,
    id: u8,
    chat: u8,
    request: Option<u8>,
    role: &str,
    deleted_at: Option<&str>,
) -> Result<(), sea_orm::DbErr> {
    let deleted = deleted_at.map_or_else(|| "NULL".to_owned(), |d| format!("'{d}'"));
    let request = request.map_or_else(|| "NULL".to_owned(), u);
    exec(
        db,
        &format!(
            "INSERT INTO messages (id, tenant_id, chat_id, request_id, role, content, created_at, deleted_at) \
             VALUES ({}, {}, {}, {request}, '{role}', 'hi', '{TS}', {deleted})",
            u(id),
            u(100),
            u(chat)
        ),
    )
    .await
}

async fn insert_attachment(
    db: &DatabaseConnection,
    id: u8,
    chat: u8,
    kind: &str,
    cleanup_status: &str,
) -> Result<(), sea_orm::DbErr> {
    exec(
        db,
        &format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, \
             content_type, size_bytes, status, attachment_kind, cleanup_status, created_at) \
             VALUES ({}, {}, {}, {}, 'a.txt', 'text/plain', 3, 'pending', '{kind}', '{cleanup_status}', '{TS}')",
            u(id),
            u(100),
            u(chat),
            u(101)
        ),
    )
    .await
}

const NO_MCP: &str = "mcp";

#[tokio::test]
async fn sqlite_migrations_create_all_tables() {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    let manager = SchemaManager::new(&db);
    for m in all_migrations() {
        m.up(&manager).await.expect("migration up");
    }
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
        assert!(tables.contains(t), "missing table {t}: {tables:?}");
    }
    assert!(
        tables
            .iter()
            .any(|t| t.starts_with(OUTBOX_TABLE_PREFIX) && t.ends_with("_incoming")),
        "outbox tables missing: {tables:?}"
    );
    assert_eq!(OUTBOX_TABLE_PREFIX, "mini_chat_outbox");
    assert!(
        tables.iter().all(|t| !t.contains(NO_MCP)),
        "no mcp tables expected: {tables:?}"
    );
}

#[tokio::test]
async fn test_db_applies_gear_and_outbox_migrations() {
    use toolkit_db::secure::SecureInsertExt;
    use toolkit_security::AccessScope;

    let dsn = super::test_dsn();
    let db = super::connect_migrated(&dsn).await;
    let conn = db.conn().expect("conn");
    let now = now_us();
    chat::Entity::insert(chat::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(Uuid::new_v4()),
        user_id: Set(Uuid::new_v4()),
        model: Set(Some("m".to_owned())),
        title: Set(None),
        is_temporary: Set(false),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
    })
    .secure()
    .scope_unchecked(&AccessScope::allow_all())
    .expect("scope")
    .exec(&conn)
    .await
    .expect("insert via secure orm");

    // A second connection to the same shared-cache in-memory database sees the
    // gear tables and the prefixed outbox tables created by `test_db` machinery.
    let raw = Database::connect(dsn.as_str()).await.expect("raw connect");
    let tables = table_names(&raw).await;
    assert!(tables.contains("chats"), "{tables:?}");
    assert!(
        tables.contains(&format!("{OUTBOX_TABLE_PREFIX}_incoming")),
        "outbox table missing: {tables:?}"
    );
    // `test_db()` itself works too.
    let _ = super::test_db().await;
}

fn ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn now_us() -> DateTime<Utc> {
    ts("2026-10-04T12:34:56.123456Z")
}

#[tokio::test]
async fn columns_match_design() {
    let db = migrated_db().await;
    // (table, [(column, sqlite type, not null)])
    let expected: &[(&str, &[(&str, &str, bool)])] = &[
        (
            "chats",
            &[
                ("id", "TEXT", true),
                ("tenant_id", "TEXT", true),
                ("user_id", "TEXT", true),
                ("model", "TEXT", false),
                ("title", "TEXT", false),
                ("is_temporary", "INTEGER", true),
                ("created_at", "TEXT", true),
                ("updated_at", "TEXT", true),
                ("deleted_at", "TEXT", false),
            ],
        ),
        (
            "messages",
            &[
                ("id", "TEXT", true),
                ("tenant_id", "TEXT", true),
                ("chat_id", "TEXT", true),
                ("request_id", "TEXT", false),
                ("role", "TEXT", true),
                ("content", "TEXT", true),
                ("content_type", "TEXT", true),
                ("token_estimate", "INTEGER", true),
                ("provider_response_id", "TEXT", false),
                ("request_kind", "TEXT", true),
                ("features_used", "TEXT", true),
                ("input_tokens", "INTEGER", true),
                ("output_tokens", "INTEGER", true),
                ("cache_read_input_tokens", "INTEGER", true),
                ("cache_write_input_tokens", "INTEGER", true),
                ("reasoning_tokens", "INTEGER", true),
                ("model", "TEXT", false),
                ("is_compressed", "INTEGER", true),
                ("created_at", "TEXT", true),
                ("deleted_at", "TEXT", false),
            ],
        ),
        (
            "chat_turns",
            &[
                ("id", "TEXT", true),
                ("tenant_id", "TEXT", true),
                ("chat_id", "TEXT", true),
                ("request_id", "TEXT", true),
                ("requester_type", "TEXT", true),
                ("requester_user_id", "TEXT", false),
                ("state", "TEXT", true),
                ("provider_name", "TEXT", false),
                ("provider_response_id", "TEXT", false),
                ("assistant_message_id", "TEXT", false),
                ("error_code", "TEXT", false),
                ("reserve_tokens", "INTEGER", false),
                ("max_output_tokens_applied", "INTEGER", false),
                ("reserved_credits_micro", "INTEGER", false),
                ("policy_version_applied", "INTEGER", false),
                ("effective_model", "TEXT", false),
                ("minimal_generation_floor_applied", "INTEGER", false),
                ("error_detail", "TEXT", false),
                ("deleted_at", "TEXT", false),
                ("replaced_by_request_id", "TEXT", false),
                ("started_at", "TEXT", true),
                ("last_progress_at", "TEXT", false),
                ("web_search_enabled", "INTEGER", true),
                ("web_search_completed_count", "INTEGER", true),
                ("code_interpreter_completed_count", "INTEGER", true),
                ("file_search_completed_count", "INTEGER", true),
                ("completed_at", "TEXT", false),
                ("updated_at", "TEXT", false),
            ],
        ),
        (
            "attachments",
            &[
                ("id", "TEXT", true),
                ("tenant_id", "TEXT", true),
                ("chat_id", "TEXT", true),
                ("uploaded_by_user_id", "TEXT", true),
                ("filename", "TEXT", true),
                ("content_type", "TEXT", false),
                ("size_bytes", "INTEGER", false),
                ("storage_backend", "TEXT", true),
                ("provider_file_id", "TEXT", false),
                ("status", "TEXT", true),
                ("error_code", "TEXT", false),
                ("attachment_kind", "TEXT", true),
                ("for_file_search", "INTEGER", true),
                ("for_code_interpreter", "INTEGER", true),
                ("doc_summary", "TEXT", false),
                ("img_thumbnail", "BLOB", false),
                ("img_thumbnail_width", "INTEGER", false),
                ("img_thumbnail_height", "INTEGER", false),
                ("summary_model", "TEXT", false),
                ("summary_updated_at", "TEXT", false),
                ("cleanup_status", "TEXT", false),
                ("cleanup_attempts", "INTEGER", true),
                ("last_cleanup_error", "TEXT", false),
                ("cleanup_updated_at", "TEXT", false),
                ("created_at", "TEXT", true),
                ("updated_at", "TEXT", true),
                ("deleted_at", "TEXT", false),
                ("secondary_file_id", "TEXT", false),
                ("secondary_status", "TEXT", true),
                ("secondary_provider_kind", "TEXT", false),
            ],
        ),
        (
            "message_attachments",
            &[
                ("tenant_id", "TEXT", true),
                ("chat_id", "TEXT", true),
                ("message_id", "TEXT", true),
                ("attachment_id", "TEXT", true),
                ("created_at", "TEXT", true),
            ],
        ),
        (
            "thread_summaries",
            &[
                ("id", "TEXT", true),
                ("tenant_id", "TEXT", true),
                ("chat_id", "TEXT", true),
                ("summary_text", "TEXT", false),
                ("summarized_up_to_created_at", "TEXT", true),
                ("summarized_up_to_message_id", "TEXT", true),
                ("token_estimate", "INTEGER", false),
                ("created_at", "TEXT", true),
                ("updated_at", "TEXT", true),
            ],
        ),
        (
            "chat_vector_stores",
            &[
                ("id", "TEXT", true),
                ("tenant_id", "TEXT", true),
                ("chat_id", "TEXT", true),
                ("vector_store_id", "TEXT", false),
                ("provider", "TEXT", true),
                ("file_count", "INTEGER", true),
                ("created_at", "TEXT", true),
            ],
        ),
        (
            "quota_usage",
            &[
                ("id", "TEXT", true),
                ("tenant_id", "TEXT", true),
                ("user_id", "TEXT", true),
                ("period_type", "TEXT", true),
                ("period_start", "TEXT", true),
                ("bucket", "TEXT", true),
                ("spent_credits_micro", "INTEGER", true),
                ("reserved_credits_micro", "INTEGER", true),
                ("calls", "INTEGER", true),
                ("input_tokens", "INTEGER", true),
                ("output_tokens", "INTEGER", true),
                ("file_search_calls", "INTEGER", true),
                ("web_search_calls", "INTEGER", true),
                ("code_interpreter_calls", "INTEGER", true),
                ("rag_retrieval_calls", "INTEGER", true),
                ("image_inputs", "INTEGER", true),
                ("image_upload_bytes", "INTEGER", true),
                ("updated_at", "TEXT", false),
            ],
        ),
        (
            "message_reactions",
            &[
                ("id", "TEXT", true),
                ("message_id", "TEXT", true),
                ("user_id", "TEXT", true),
                ("tenant_id", "TEXT", true),
                ("reaction", "TEXT", true),
                ("created_at", "TEXT", true),
            ],
        ),
    ];
    for (table, cols) in expected {
        let actual: BTreeSet<(String, String, bool)> =
            columns(&db, table).await.into_iter().collect();
        let want: BTreeSet<(String, String, bool)> = cols
            .iter()
            .map(|(n, t, nn)| ((*n).to_owned(), (*t).to_owned(), *nn))
            .collect();
        assert_eq!(actual, want, "column set mismatch for {table}");
    }
}

#[tokio::test]
async fn one_running_turn_per_chat_index() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await;
    insert_chat(&db, 2).await;
    insert_turn(&db, 10, 1, 50, "running", None)
        .await
        .expect("first running");
    // second running turn for the same chat fails
    assert!(insert_turn(&db, 11, 1, 51, "running", None).await.is_err());
    // another chat is independent
    insert_turn(&db, 12, 2, 52, "running", None)
        .await
        .expect("other chat");
    // a finished turn does not count
    insert_turn(&db, 13, 1, 53, "completed", None)
        .await
        .expect("completed turn");
    // after the first running turn is soft-deleted a new one succeeds
    exec(
        &db,
        &format!(
            "UPDATE chat_turns SET deleted_at = '{TS}' WHERE id = {}",
            u(10)
        ),
    )
    .await
    .unwrap();
    insert_turn(&db, 14, 1, 54, "running", None)
        .await
        .expect("running after soft delete");
}

#[tokio::test]
async fn turn_request_id_unique_per_chat() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await;
    insert_chat(&db, 2).await;
    insert_turn(&db, 10, 1, 50, "completed", None)
        .await
        .unwrap();
    assert!(
        insert_turn(&db, 11, 1, 50, "completed", None)
            .await
            .is_err()
    );
    // soft-deleted turns still hold the request id
    exec(&db, &format!("UPDATE chat_turns SET deleted_at = '{TS}'"))
        .await
        .unwrap();
    assert!(
        insert_turn(&db, 12, 1, 50, "completed", None)
            .await
            .is_err()
    );
    // same request id in another chat is fine
    insert_turn(&db, 13, 2, 50, "completed", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn message_request_role_unique_for_live_rows() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await;
    insert_message(&db, 10, 1, Some(50), "user", None)
        .await
        .unwrap();
    // one assistant message per request id is allowed
    insert_message(&db, 11, 1, Some(50), "assistant", None)
        .await
        .unwrap();
    // live duplicate fails
    assert!(
        insert_message(&db, 12, 1, Some(50), "user", None)
            .await
            .is_err()
    );
    // duplicate with deleted_at set is ok
    insert_message(&db, 13, 1, Some(50), "user", Some(TS))
        .await
        .unwrap();
    insert_message(&db, 14, 1, Some(50), "user", Some(TS))
        .await
        .unwrap();
    // rows without request_id are never constrained
    insert_message(&db, 15, 1, None, "user", None)
        .await
        .unwrap();
    insert_message(&db, 16, 1, None, "user", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn quota_usage_bucket_unique() {
    let db = migrated_db().await;
    let ins = |id: u8, bucket: &str, start: &str| {
        let sql = format!(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, updated_at) \
             VALUES ({}, {}, {}, 'daily', '{start}', '{bucket}', '{TS}')",
            u(id),
            u(100),
            u(101)
        );
        let db = &db;
        async move { exec(db, &sql).await }
    };
    ins(1, "total", "2026-10-04").await.unwrap();
    assert!(ins(2, "total", "2026-10-04").await.is_err());
    ins(3, "tier:premium", "2026-10-04").await.unwrap();
    ins(4, "total", "2026-10-05").await.unwrap();
    // defaults are zero
    let rows = quota_usage::Entity::find().all(&db).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .all(|r| r.spent_credits_micro == 0 && r.calls == 0 && r.image_upload_bytes == 0)
    );
}

#[tokio::test]
async fn reaction_unique_per_user_message() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await;
    insert_message(&db, 10, 1, None, "assistant", None)
        .await
        .unwrap();
    let ins = |id: u8, user: u8, reaction: &str| {
        let sql = format!(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, created_at) \
             VALUES ({}, {}, {}, {}, '{reaction}', '{TS}')",
            u(id),
            u(10),
            u(user),
            u(100)
        );
        let db = &db;
        async move { exec(db, &sql).await }
    };
    ins(1, 101, "like").await.unwrap();
    assert!(ins(2, 101, "dislike").await.is_err());
    ins(3, 102, "like").await.unwrap();
}

#[tokio::test]
async fn vector_store_unique_per_chat() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await;
    let ins = |id: u8, tenant: u8, chat: u8, vs: Option<&str>| {
        let vs = vs.map_or_else(|| "NULL".to_owned(), |v| format!("'{v}'"));
        let sql = format!(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, created_at) \
             VALUES ({}, {}, {}, {vs}, 'azure', '{TS}')",
            u(id),
            u(tenant),
            u(chat)
        );
        let db = &db;
        async move { exec(db, &sql).await }
    };
    // placeholder row (vector_store_id NULL) is allowed
    ins(1, 100, 1, None).await.unwrap();
    assert!(ins(2, 100, 1, Some("vs_1")).await.is_err());
    // no FK to chats: a different tenant/chat pair is simply another row
    ins(3, 200, 1, Some("vs_2")).await.unwrap();
    // file_count >= 0
    let bad = exec(
        &db,
        &format!(
            "UPDATE chat_vector_stores SET file_count = -1 WHERE id = {}",
            u(1)
        ),
    )
    .await;
    assert!(bad.is_err());
}

#[tokio::test]
async fn check_constraints_enforced() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await;
    assert!(insert_turn(&db, 10, 1, 50, "bogus", None).await.is_err());
    assert!(
        insert_attachment(&db, 20, 1, "video", "pending")
            .await
            .is_err()
    );
    // cleanup_status value set is NOT enforced
    insert_attachment(&db, 21, 1, "image", "weird")
        .await
        .unwrap();
    insert_attachment(&db, 22, 1, "document", "pending")
        .await
        .unwrap();

    insert_message(&db, 30, 1, None, "assistant", None)
        .await
        .unwrap();
    let react = |reaction: &str| {
        let sql = format!(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, created_at) \
             VALUES ({}, {}, {}, {}, '{reaction}', '{TS}')",
            u(40),
            u(30),
            u(101),
            u(100)
        );
        let db = &db;
        async move { exec(db, &sql).await }
    };
    assert!(react("love").await.is_err());
    react("like").await.unwrap();

    // requester_type
    let bad_requester = exec(
        &db,
        &format!(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, state, started_at, updated_at) \
             VALUES ({}, {}, {}, {}, 'robot', 'completed', '{TS}', '{TS}')",
            u(11),
            u(100),
            u(1),
            u(51)
        ),
    )
    .await;
    assert!(bad_requester.is_err());
    // attachment status / secondary columns
    for (col, val) in [
        ("status", "bogus"),
        ("secondary_status", "bogus"),
        ("secondary_provider_kind", "openai"),
    ] {
        let r = exec(
            &db,
            &format!(
                "UPDATE attachments SET {col} = '{val}' WHERE id = {}",
                u(22)
            ),
        )
        .await;
        assert!(r.is_err(), "{col}={val} must be rejected");
    }
    exec(&db, &format!("UPDATE attachments SET secondary_provider_kind = 'anthropic', secondary_status = 'uploaded' WHERE id = {}", u(22)))
        .await
        .unwrap();
    // turn cross-column CHECKs are NOT enforced: completed without completed_at, running without progress
    insert_turn(&db, 12, 1, 52, "completed", None)
        .await
        .unwrap();
    exec(
        &db,
        &format!(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, state, started_at, updated_at) \
             VALUES ({}, {}, {}, {}, 'user', 'running', '{TS}', '{TS}')",
            u(13),
            u(100),
            u(1),
            u(53)
        ),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn message_attachment_composite_fk_rejects_cross_chat() {
    let db = migrated_db().await;
    insert_chat(&db, 1).await;
    insert_chat(&db, 2).await;
    insert_message(&db, 10, 1, None, "user", None)
        .await
        .unwrap();
    insert_attachment(&db, 20, 1, "document", "pending")
        .await
        .unwrap();
    insert_attachment(&db, 21, 2, "document", "pending")
        .await
        .unwrap();

    let link = |chat: u8, msg: u8, att: u8| {
        let sql = format!(
            "INSERT INTO message_attachments (tenant_id, chat_id, message_id, attachment_id, created_at) \
             VALUES ({}, {}, {}, {}, '{TS}')",
            u(100),
            u(chat),
            u(msg),
            u(att)
        );
        let db = &db;
        async move { exec(db, &sql).await }
    };
    // attachment from chat 2 linked with chat 1 message: rejected
    assert!(link(1, 10, 21).await.is_err());
    // chat id that does not match the message: rejected
    assert!(link(2, 10, 21).await.is_err());
    link(1, 10, 20).await.unwrap();

    // the FKs are declared (and cascade)
    let fks = db
        .query_all_raw(stmt(&db, "PRAGMA foreign_key_list(message_attachments)"))
        .await
        .unwrap();
    let declared: BTreeSet<(String, String)> = fks
        .iter()
        .map(|r| {
            (
                r.try_get::<String>("", "table").unwrap(),
                r.try_get::<String>("", "on_delete").unwrap(),
            )
        })
        .collect();
    assert_eq!(
        declared,
        BTreeSet::from([
            ("messages".to_owned(), "CASCADE".to_owned()),
            ("attachments".to_owned(), "CASCADE".to_owned())
        ])
    );

    // deleting the chat cascades to messages, attachments and links
    exec(&db, &format!("DELETE FROM chats WHERE id = {}", u(1)))
        .await
        .unwrap();
    let left = db
        .query_one_raw(stmt(&db, "SELECT COUNT(*) AS c FROM message_attachments"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(left.try_get::<i64>("", "c").unwrap(), 0);
}

/// Reads one column of the single row of `table` as text (`NULL` -> `None`).
async fn text_of(db: &DatabaseConnection, table: &str, col: &str) -> Option<String> {
    let row = db
        .query_one_raw(stmt(
            db,
            format!("SELECT CAST({col} AS TEXT) AS v FROM {table}"),
        ))
        .await
        .expect("select")
        .expect("row");
    row.try_get::<Option<String>>("", "v").expect("value")
}

/// `YYYY-MM-DDTHH:MM:SS[.fff[fff[fff]]]+00:00`: the text sqlx writes for
/// `chrono::DateTime<Utc>` on `SQLite`.
fn is_chrono_utc_text(raw: &str) -> bool {
    let Some(body) = raw.strip_suffix("+00:00") else {
        return false;
    };
    let (secs, frac) = body.split_once('.').unwrap_or((body, ""));
    secs.len() == 19
        && secs.as_bytes()[10] == b'T'
        && [0, 3, 6, 9].contains(&frac.len())
        && frac.bytes().all(|b| b.is_ascii_digit())
        && DateTime::parse_from_rfc3339(raw).is_ok()
}

#[tokio::test]
async fn timestamps_are_written_as_chrono_utc_text() {
    let db = migrated_db().await;
    for (value, want) in [
        (
            "2026-10-04T12:34:56.123456Z",
            "2026-10-04T12:34:56.123456+00:00",
        ),
        ("2026-10-04T12:34:56.5Z", "2026-10-04T12:34:56.500+00:00"),
        ("2026-10-04T12:34:56Z", "2026-10-04T12:34:56+00:00"),
    ] {
        exec(&db, "DELETE FROM chats").await.unwrap();
        let t = ts(value);
        chat::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(Uuid::new_v4()),
            user_id: Set(Uuid::new_v4()),
            model: Set(None),
            title: Set(None),
            is_temporary: Set(false),
            created_at: Set(t),
            updated_at: Set(t),
            deleted_at: Set(None),
        }
        .insert(&db)
        .await
        .unwrap();
        let row = db
            .query_one_raw(stmt(
                &db,
                "SELECT typeof(updated_at) AS t, updated_at AS v FROM chats",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<String>("", "t").unwrap(), "text");
        let raw = row.try_get::<String>("", "v").unwrap();
        assert_eq!(raw, want);
        assert!(is_chrono_utc_text(&raw), "{raw}");
    }
}

#[tokio::test]
async fn defaults_apply_to_minimal_rows() {
    let db = migrated_db().await;
    // Only columns that are NOT NULL without a default (D§3.7).
    exec(
        &db,
        &format!(
            "INSERT INTO chats (id, tenant_id, user_id, created_at, updated_at) \
             VALUES ({}, {}, {}, '{TS}', '{TS}')",
            u(1),
            u(100),
            u(101)
        ),
    )
    .await
    .unwrap();
    let defaults: &[(&str, &[(&str, Option<&str>)])] = &[(
        "chats",
        &[
            ("is_temporary", Some("0")),
            ("model", None),
            ("title", None),
            ("deleted_at", None),
        ],
    )];
    for (table, cols) in defaults {
        for (col, want) in *cols {
            assert_eq!(
                text_of(&db, table, col).await.as_deref(),
                *want,
                "{table}.{col}"
            );
        }
    }

    exec(
        &db,
        &format!(
            "INSERT INTO messages (id, tenant_id, chat_id, role, content, created_at) \
             VALUES ({}, {}, {}, 'user', 'x', '{TS}')",
            u(2),
            u(100),
            u(1)
        ),
    )
    .await
    .unwrap();
    for (col, want) in [
        ("content_type", Some("text")),
        ("request_kind", Some("chat")),
        ("features_used", Some("[]")),
        ("token_estimate", Some("0")),
        ("input_tokens", Some("0")),
        ("output_tokens", Some("0")),
        ("cache_read_input_tokens", Some("0")),
        ("cache_write_input_tokens", Some("0")),
        ("reasoning_tokens", Some("0")),
        ("is_compressed", Some("0")),
        ("request_id", None),
        ("model", None),
        ("provider_response_id", None),
        ("deleted_at", None),
    ] {
        assert_eq!(
            text_of(&db, "messages", col).await.as_deref(),
            want,
            "messages.{col}"
        );
    }

    // chat_turns: D§3.7 gives no explicit NOT NULL list; identity/state columns are required.
    exec(
        &db,
        &format!(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, state, started_at) \
             VALUES ({}, {}, {}, {}, 'user', 'running', '{TS}')",
            u(3),
            u(100),
            u(1),
            u(50)
        ),
    )
    .await
    .unwrap();
    for (col, want) in [
        ("web_search_enabled", Some("0")),
        ("web_search_completed_count", Some("0")),
        ("code_interpreter_completed_count", Some("0")),
        ("file_search_completed_count", Some("0")),
        ("reserve_tokens", None),
        ("effective_model", None),
        ("last_progress_at", None),
        ("completed_at", None),
        ("updated_at", None),
        ("deleted_at", None),
    ] {
        assert_eq!(
            text_of(&db, "chat_turns", col).await.as_deref(),
            want,
            "chat_turns.{col}"
        );
    }

    exec(
        &db,
        &format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, status, attachment_kind, created_at) \
             VALUES ({}, {}, {}, {}, 'f', 'pending', 'document', '{TS}')",
            u(4),
            u(100),
            u(1),
            u(101)
        ),
    )
    .await
    .unwrap();
    for (col, want) in [
        ("storage_backend", Some("azure")),
        ("secondary_status", Some("not_attempted")),
        ("for_file_search", Some("0")),
        ("for_code_interpreter", Some("0")),
        ("cleanup_attempts", Some("0")),
        ("content_type", None),
        ("size_bytes", None),
        ("cleanup_status", None),
        ("secondary_provider_kind", None),
        ("deleted_at", None),
    ] {
        assert_eq!(
            text_of(&db, "attachments", col).await.as_deref(),
            want,
            "attachments.{col}"
        );
    }
    // `updated_at` default has the shape sea-orm writes for `DateTimeUtc` and is
    // readable by the entity.
    let att = attachment::Entity::find().one(&db).await.unwrap().unwrap();
    let raw = text_of(&db, "attachments", "updated_at").await.unwrap();
    assert!(is_chrono_utc_text(&raw), "unexpected default format {raw}");
    assert!(att.updated_at.year() >= 2026);

    exec(
        &db,
        &format!(
            "INSERT INTO message_attachments (tenant_id, chat_id, message_id, attachment_id, created_at) \
             VALUES ({}, {}, {}, {}, '{TS}')",
            u(100),
            u(1),
            u(2),
            u(4)
        ),
    )
    .await
    .unwrap();

    exec(
        &db,
        &format!(
            "INSERT INTO thread_summaries (id, tenant_id, chat_id, summarized_up_to_created_at, \
             summarized_up_to_message_id, created_at, updated_at) \
             VALUES ({}, {}, {}, '{TS}', {}, '{TS}', '{TS}')",
            u(5),
            u(100),
            u(1),
            u(2)
        ),
    )
    .await
    .unwrap();
    assert_eq!(text_of(&db, "thread_summaries", "summary_text").await, None);
    assert_eq!(
        text_of(&db, "thread_summaries", "token_estimate").await,
        None
    );

    exec(
        &db,
        &format!(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, provider, created_at) \
             VALUES ({}, {}, {}, 'azure', '{TS}')",
            u(6),
            u(100),
            u(1)
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        text_of(&db, "chat_vector_stores", "file_count")
            .await
            .as_deref(),
        Some("0")
    );
    assert_eq!(
        text_of(&db, "chat_vector_stores", "vector_store_id").await,
        None
    );

    exec(
        &db,
        &format!(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket) \
             VALUES ({}, {}, {}, 'daily', '2026-10-04', 'total')",
            u(7),
            u(100),
            u(101)
        ),
    )
    .await
    .unwrap();
    for col in [
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
            text_of(&db, "quota_usage", col).await.as_deref(),
            Some("0"),
            "quota_usage.{col}"
        );
    }
    assert_eq!(text_of(&db, "quota_usage", "updated_at").await, None);

    exec(
        &db,
        &format!(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, created_at) \
             VALUES ({}, {}, {}, {}, 'like', '{TS}')",
            u(8),
            u(2),
            u(101),
            u(100)
        ),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn entities_roundtrip() {
    let db = migrated_db().await;
    let now = now_us();
    let later = ts("2026-10-04T13:00:00.654321Z");
    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    let chat_id = Uuid::new_v4();
    let msg_id = Uuid::new_v4();
    let att_id = Uuid::new_v4();
    let req = Uuid::new_v4();

    let c = chat::ActiveModel {
        id: Set(chat_id),
        tenant_id: Set(tenant),
        user_id: Set(user),
        model: Set(Some("gpt-5".to_owned())),
        title: Set(Some("hello".to_owned())),
        is_temporary: Set(false),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
    }
    .insert(&db)
    .await
    .unwrap();
    assert_eq!(
        chat::Entity::find_by_id(chat_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        c
    );
    assert_eq!(c.created_at, now);

    let m = message::ActiveModel {
        id: Set(msg_id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        request_id: Set(Some(req)),
        role: Set("user".to_owned()),
        content: Set("hi".to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(1),
        output_tokens: Set(2),
        cache_read_input_tokens: Set(3),
        cache_write_input_tokens: Set(4),
        reasoning_tokens: Set(5),
        model: Set(Some("gpt-5".to_owned())),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(Some(later)),
    }
    .insert(&db)
    .await
    .unwrap();
    let got = message::Entity::find_by_id(msg_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, m);
    assert_eq!(got.features_used, json!([]));
    assert_eq!(got.deleted_at, Some(later));
    // raw TEXT form of the JSON column on SQLite
    let raw = db
        .query_one_raw(stmt(
            &db,
            "SELECT features_used AS f, typeof(features_used) AS t FROM messages",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(raw.try_get::<String>("", "f").unwrap(), "[]");
    assert_eq!(raw.try_get::<String>("", "t").unwrap(), "text");

    let t = chat_turn::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        request_id: Set(req),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(user)),
        state: Set("running".to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(Some("resp_1".to_owned())),
        assistant_message_id: Set(Some(msg_id)),
        error_code: Set(None),
        reserve_tokens: Set(Some(100)),
        max_output_tokens_applied: Set(Some(50)),
        reserved_credits_micro: Set(Some(7)),
        policy_version_applied: Set(Some(3)),
        effective_model: Set(Some("gpt-5".to_owned())),
        minimal_generation_floor_applied: Set(Some(10)),
        error_detail: Set(None),
        deleted_at: Set(None),
        replaced_by_request_id: Set(Some(Uuid::new_v4())),
        started_at: Set(now),
        last_progress_at: Set(Some(later)),
        web_search_enabled: Set(true),
        web_search_completed_count: Set(1),
        code_interpreter_completed_count: Set(2),
        file_search_completed_count: Set(3),
        completed_at: Set(None),
        updated_at: Set(Some(later)),
    }
    .insert(&db)
    .await
    .unwrap();
    assert_eq!(
        chat_turn::Entity::find_by_id(t.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        t
    );

    let a = attachment::ActiveModel {
        id: Set(att_id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(user),
        filename: Set("a.png".to_owned()),
        content_type: Set(Some("image/png".to_owned())),
        size_bytes: Set(Some(1234)),
        storage_backend: Set("azure".to_owned()),
        provider_file_id: Set(Some("file-1".to_owned())),
        status: Set("ready".to_owned()),
        error_code: Set(None),
        attachment_kind: Set("image".to_owned()),
        for_file_search: Set(false),
        for_code_interpreter: Set(true),
        doc_summary: Set(None),
        img_thumbnail: Set(Some(vec![0, 1, 2, 255])),
        img_thumbnail_width: Set(Some(64)),
        img_thumbnail_height: Set(Some(32)),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(Some("pending".to_owned())),
        cleanup_attempts: Set(2),
        last_cleanup_error: Set(Some("boom".to_owned())),
        cleanup_updated_at: Set(Some(later)),
        created_at: Set(now),
        updated_at: Set(later),
        deleted_at: Set(None),
        secondary_file_id: Set(None),
        secondary_status: Set("not_attempted".to_owned()),
        secondary_provider_kind: Set(None),
    }
    .insert(&db)
    .await
    .unwrap();
    assert_eq!(
        attachment::Entity::find_by_id(att_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        a
    );

    let ma = message_attachment::ActiveModel {
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        message_id: Set(msg_id),
        attachment_id: Set(att_id),
        created_at: Set(now),
    }
    .insert(&db)
    .await
    .unwrap();
    assert_eq!(
        message_attachment::Entity::find_by_id((chat_id, msg_id, att_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        ma
    );

    let s = thread_summary::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        summary_text: Set(Some("sum".to_owned())),
        summarized_up_to_created_at: Set(now),
        summarized_up_to_message_id: Set(msg_id),
        token_estimate: Set(Some(12)),
        created_at: Set(now),
        updated_at: Set(later),
    }
    .insert(&db)
    .await
    .unwrap();
    assert_eq!(
        thread_summary::Entity::find_by_id(s.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        s
    );
    assert_eq!(s.summarized_up_to_created_at, now);

    let v = chat_vector_store::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        vector_store_id: Set(None),
        provider: Set("azure".to_owned()),
        file_count: Set(0),
        created_at: Set(now),
    }
    .insert(&db)
    .await
    .unwrap();
    assert_eq!(
        chat_vector_store::Entity::find_by_id(v.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        v
    );

    let period = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
    let q = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant),
        user_id: Set(user),
        period_type: Set("daily".to_owned()),
        period_start: Set(period),
        bucket: Set("total".to_owned()),
        spent_credits_micro: Set(10),
        reserved_credits_micro: Set(20),
        calls: Set(1),
        input_tokens: Set(2),
        output_tokens: Set(3),
        file_search_calls: Set(0),
        web_search_calls: Set(4),
        code_interpreter_calls: Set(5),
        rag_retrieval_calls: Set(0),
        image_inputs: Set(0),
        image_upload_bytes: Set(0),
        updated_at: Set(Some(later)),
    }
    .insert(&db)
    .await
    .unwrap();
    let got = quota_usage::Entity::find_by_id(q.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, q);
    assert_eq!(got.period_start, period);
    let raw = db
        .query_one_raw(stmt(&db, "SELECT period_start AS p FROM quota_usage"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(raw.try_get::<String>("", "p").unwrap(), "2026-10-04");

    let r = message_reaction::ActiveModel {
        id: Set(Uuid::new_v4()),
        message_id: Set(msg_id),
        user_id: Set(user),
        tenant_id: Set(tenant),
        reaction: Set("like".to_owned()),
        created_at: Set(now),
    }
    .insert(&db)
    .await
    .unwrap();
    assert_eq!(
        message_reaction::Entity::find_by_id(r.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        r
    );

    // UUIDs are stored as 16-byte blobs
    let raw = db
        .query_one_raw(stmt(
            &db,
            "SELECT typeof(id) AS t, length(id) AS l FROM chats",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(raw.try_get::<String>("", "t").unwrap(), "blob");
    assert_eq!(raw.try_get::<i64>("", "l").unwrap(), 16);
}

#[test]
fn postgres_sql_variant_is_provided() {
    let statements = m0001_initial::pg_statements();
    assert!(!statements.is_empty());
    let all = statements.join("\n");
    for needle in [
        "TIMESTAMPTZ",
        "UUID",
        "JSONB",
        "BYTEA",
        "DATE",
        "BOOLEAN",
        "WHERE state = 'running' AND deleted_at IS NULL",
    ] {
        assert!(all.contains(needle), "pg SQL must contain {needle}");
    }
    assert!(!all.contains("BLOB"));
    let sqlite = m0001_initial::sqlite_statements().join("\n");
    assert!(sqlite.contains("WHERE state = 'running' AND deleted_at IS NULL"));
    assert!(!sqlite.contains("JSONB") && !sqlite.contains("TIMESTAMPTZ"));
}
