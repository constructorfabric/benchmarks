//! Schema tests of the P1 migrations on in-memory `SQLite`: the migrations
//! apply (with the outbox migrations) through the platform runner, the
//! partial and composite unique keys, the composite foreign keys of
//! `message_attachments`, the value CHECKs, UUID storage as 16-byte BLOBs,
//! entity / table column parity and the unique-violation mapping.
//!
//! PostgreSQL is covered by the statement-list checks only (no container in
//! unit tests).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;

use sea_orm::{
    ActiveValue::Set, ConnectionTrait, Database, DatabaseConnection, DbBackend, DbErr, EntityTrait,
    IdenStatic, Iterable, RuntimeErr, Statement,
};
use sea_orm_migration::{MigrationTrait, MigratorTrait};
use time::OffsetDateTime;
use toolkit_db::DbError;
use uuid::Uuid;

use super::entities::{
    DbTimestamp, TimestampForm, attachments, chat_turns, chat_vector_stores, chats,
    message_attachments, message_reactions, messages, quota_usage, thread_summaries,
};
use super::error::{DbTable, UniqueTarget, unique_violation_target};
use super::migrations::{Migrator, all_statements, migrations};
use crate::domain::error::{DomainError, ResourceKind, conflict};

/// Migrator over the full set (gear tables plus outbox).
struct AllMigrations;

impl MigratorTrait for AllMigrations {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        migrations()
    }
}

const GEAR_TABLES: [&str; 9] = [
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

fn stmt(sql: &str) -> Statement {
    Statement::from_string(DbBackend::Sqlite, sql.to_owned())
}

/// Fresh in-memory `SQLite` (single connection) with every migration applied
/// and foreign keys enforced.
async fn migrated_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db.execute_unprepared("PRAGMA foreign_keys = ON")
        .await
        .expect("enable foreign keys");
    AllMigrations::up(&db, None)
        .await
        .expect("apply all migrations");
    db
}

async fn table_names(db: &DatabaseConnection) -> BTreeSet<String> {
    db.query_all_raw(stmt("SELECT name FROM sqlite_master WHERE type = 'table'"))
        .await
        .expect("list tables")
        .into_iter()
        .map(|row| row.try_get::<String>("", "name").expect("table name"))
        .collect()
}

fn now() -> DbTimestamp {
    TimestampForm::FixedText.stamp(OffsetDateTime::now_utc())
}

async fn insert_chat(db: &DatabaseConnection, tenant_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let model = chats::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        user_id: Set(Uuid::new_v4()),
        model: Set("gpt-test".to_owned()),
        title: Set(Some("title".to_owned())),
        is_temporary: Set(false),
        created_at: Set(now()),
        updated_at: Set(now()),
        deleted_at: Set(None),
    };
    chats::Entity::insert(model)
        .exec_without_returning(db)
        .await
        .expect("insert chat");
    id
}

async fn insert_turn(
    db: &DatabaseConnection,
    chat_id: Uuid,
    request_id: Uuid,
    state: &str,
    deleted: bool,
) -> Result<u64, DbErr> {
    let completed_at = (state != "running").then(now);
    let model = chat_turns::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        request_id: Set(request_id),
        requester_type: Set("user".to_owned()),
        state: Set(state.to_owned()),
        deleted_at: Set(deleted.then(now)),
        started_at: Set(now()),
        last_progress_at: Set(Some(now())),
        completed_at: Set(completed_at),
        updated_at: Set(now()),
        ..Default::default()
    };
    chat_turns::Entity::insert(model)
        .exec_without_returning(db)
        .await
}

async fn insert_message(
    db: &DatabaseConnection,
    chat_id: Uuid,
    request_id: Option<Uuid>,
    role: &str,
    deleted: bool,
) -> Result<Uuid, DbErr> {
    let id = Uuid::new_v4();
    let model = messages::ActiveModel {
        id: Set(id),
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        request_id: Set(request_id),
        role: Set(role.to_owned()),
        content: Set("hello".to_owned()),
        created_at: Set(now()),
        deleted_at: Set(deleted.then(now)),
        ..Default::default()
    };
    messages::Entity::insert(model)
        .exec_without_returning(db)
        .await
        .map(|_| id)
}

async fn insert_attachment(db: &DatabaseConnection, chat_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let model = attachments::ActiveModel {
        id: Set(id),
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(Uuid::new_v4()),
        filename: Set("doc.pdf".to_owned()),
        content_type: Set(Some("application/pdf".to_owned())),
        status: Set("ready".to_owned()),
        attachment_kind: Set("document".to_owned()),
        created_at: Set(now()),
        ..Default::default()
    };
    attachments::Entity::insert(model)
        .exec_without_returning(db)
        .await
        .expect("insert attachment");
    id
}

async fn link_attachment(
    db: &DatabaseConnection,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_id: Uuid,
) -> Result<u64, DbErr> {
    let model = message_attachments::ActiveModel {
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        message_id: Set(message_id),
        attachment_id: Set(attachment_id),
        created_at: Set(now()),
    };
    message_attachments::Entity::insert(model)
        .exec_without_returning(db)
        .await
}

async fn insert_reaction(
    db: &DatabaseConnection,
    message_id: Uuid,
    user_id: Uuid,
    reaction: &str,
) -> Result<u64, DbErr> {
    let model = message_reactions::ActiveModel {
        id: Set(Uuid::new_v4()),
        message_id: Set(message_id),
        user_id: Set(user_id),
        tenant_id: Set(Uuid::new_v4()),
        reaction: Set(reaction.to_owned()),
        created_at: Set(now()),
    };
    message_reactions::Entity::insert(model)
        .exec_without_returning(db)
        .await
}

fn entity_columns<E: EntityTrait>() -> BTreeSet<String> {
    E::Column::iter()
        .map(|column| column.as_str().to_owned())
        .collect()
}

async fn table_columns(db: &DatabaseConnection, table: &str) -> BTreeSet<String> {
    db.query_all_raw(stmt(&format!("PRAGMA table_info({table})")))
        .await
        .expect("table info")
        .into_iter()
        .map(|row| row.try_get::<String>("", "name").expect("column name"))
        .collect()
}

// ── Migration application ────────────────────────────────────────────────

#[tokio::test]
async fn all_migrations_apply_through_the_platform_runner() {
    let dsn = format!(
        "sqlite:file:mini-chat-{}?mode=memory&cache=shared",
        Uuid::new_v4()
    );
    let db = toolkit_db::connect_db(&dsn, toolkit_db::ConnectOpts::default())
        .await
        .expect("connect sqlite");

    let expected = migrations().len();
    assert!(
        expected > Migrator::migrations().len(),
        "the full set includes the outbox migrations"
    );

    let first = toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations())
        .await
        .expect("run migrations");
    assert_eq!(first.applied, expected);

    let second = toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations())
        .await
        .expect("re-run migrations");
    assert_eq!(second.applied, 0, "migrations are recorded once applied");
}

#[tokio::test]
async fn migrations_create_every_p1_table_and_no_mcp_table() {
    let db = migrated_db().await;
    let tables = table_names(&db).await;

    for table in GEAR_TABLES {
        assert!(tables.contains(table), "missing table {table}");
    }
    assert!(
        tables.iter().any(|name| name.starts_with("toolkit_outbox")),
        "outbox tables are created by the outbox migrations: {tables:?}"
    );
    assert!(
        !tables.iter().any(|name| name.contains("mcp")),
        "no MCP table is created: {tables:?}"
    );
}

#[tokio::test]
async fn gear_migrations_roll_back() {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    Migrator::up(&db, None).await.expect("up");
    Migrator::down(&db, None).await.expect("down");

    let tables = table_names(&db).await;
    for table in GEAR_TABLES {
        assert!(!tables.contains(table), "{table} survived down");
    }
}

#[tokio::test]
async fn entities_match_the_table_columns() {
    let db = migrated_db().await;

    let cases = [
        ("chats", entity_columns::<chats::Entity>()),
        ("messages", entity_columns::<messages::Entity>()),
        ("chat_turns", entity_columns::<chat_turns::Entity>()),
        ("attachments", entity_columns::<attachments::Entity>()),
        (
            "message_attachments",
            entity_columns::<message_attachments::Entity>(),
        ),
        (
            "thread_summaries",
            entity_columns::<thread_summaries::Entity>(),
        ),
        (
            "chat_vector_stores",
            entity_columns::<chat_vector_stores::Entity>(),
        ),
        ("quota_usage", entity_columns::<quota_usage::Entity>()),
        (
            "message_reactions",
            entity_columns::<message_reactions::Entity>(),
        ),
    ];
    for (table, columns) in cases {
        assert_eq!(table_columns(&db, table).await, columns, "table {table}");
    }
}

// ── UUID storage ─────────────────────────────────────────────────────────

#[tokio::test]
async fn uuids_are_stored_as_16_byte_blobs() {
    let db = migrated_db().await;
    let tenant_id = Uuid::new_v4();
    let chat_id = insert_chat(&db, tenant_id).await;

    let row = db
        .query_one_raw(stmt(
            "SELECT typeof(id) AS id_type, length(id) AS id_len, \
             typeof(tenant_id) AS tenant_type, id FROM chats",
        ))
        .await
        .expect("query")
        .expect("one chat");
    assert_eq!(row.try_get::<String>("", "id_type").unwrap(), "blob");
    assert_eq!(row.try_get::<i64>("", "id_len").unwrap(), 16);
    assert_eq!(row.try_get::<String>("", "tenant_type").unwrap(), "blob");
    assert_eq!(row.try_get::<Uuid>("", "id").unwrap(), chat_id);

    // A query binding a UUID directly binds its bytes.
    let by_bytes = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT count(*) AS n FROM chats WHERE id = ?",
            [chat_id.as_bytes().to_vec().into()],
        ))
        .await
        .expect("query")
        .expect("count row");
    assert_eq!(by_bytes.try_get::<i64>("", "n").unwrap(), 1);
}

// ── chat_turns ───────────────────────────────────────────────────────────

#[tokio::test]
async fn only_one_running_turn_per_chat() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;

    insert_turn(&db, chat_id, Uuid::new_v4(), "running", false)
        .await
        .expect("first running turn");
    let err = insert_turn(&db, chat_id, Uuid::new_v4(), "running", false)
        .await
        .expect_err("second running turn violates the partial unique index");
    assert_eq!(
        unique_violation_target(&err),
        Some(UniqueTarget::RunningTurn)
    );

    // A running turn in another chat is unaffected.
    let other_chat = insert_chat(&db, Uuid::new_v4()).await;
    insert_turn(&db, other_chat, Uuid::new_v4(), "running", false)
        .await
        .expect("running turn of another chat");
}

#[tokio::test]
async fn completed_and_running_turns_coexist() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;

    insert_turn(&db, chat_id, Uuid::new_v4(), "completed", false)
        .await
        .expect("completed turn");
    insert_turn(&db, chat_id, Uuid::new_v4(), "running", true)
        .await
        .expect("soft-deleted running turn");
    insert_turn(&db, chat_id, Uuid::new_v4(), "running", false)
        .await
        .expect("running turn next to completed and soft-deleted ones");
}

#[tokio::test]
async fn request_id_is_unique_per_chat() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;
    let request_id = Uuid::new_v4();

    insert_turn(&db, chat_id, request_id, "completed", false)
        .await
        .expect("first turn");
    let err = insert_turn(&db, chat_id, request_id, "failed", true)
        .await
        .expect_err("same (chat_id, request_id)");
    assert_eq!(
        unique_violation_target(&err),
        Some(UniqueTarget::TurnRequestId)
    );

    let other_chat = insert_chat(&db, Uuid::new_v4()).await;
    insert_turn(&db, other_chat, request_id, "completed", false)
        .await
        .expect("same request_id in another chat");
}

#[tokio::test]
async fn turn_value_checks_are_enforced_but_cross_column_checks_are_not() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;

    insert_turn(&db, chat_id, Uuid::new_v4(), "exploded", false)
        .await
        .expect_err("state CHECK");

    // A terminal turn without `completed_at` is accepted (ADR-0010).
    let model = chat_turns::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        request_id: Set(Uuid::new_v4()),
        requester_type: Set("system".to_owned()),
        state: Set("failed".to_owned()),
        started_at: Set(now()),
        completed_at: Set(None),
        updated_at: Set(now()),
        ..Default::default()
    };
    chat_turns::Entity::insert(model)
        .exec_without_returning(&db)
        .await
        .expect("cross-column CHECKs are not enforced");

    let bad_requester = chat_turns::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        request_id: Set(Uuid::new_v4()),
        requester_type: Set("robot".to_owned()),
        state: Set("completed".to_owned()),
        started_at: Set(now()),
        updated_at: Set(now()),
        ..Default::default()
    };
    chat_turns::Entity::insert(bad_requester)
        .exec_without_returning(&db)
        .await
        .expect_err("requester_type CHECK");
}

// ── messages ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn message_request_role_is_unique_among_live_messages() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;
    let request_id = Uuid::new_v4();

    insert_message(&db, chat_id, Some(request_id), "user", false)
        .await
        .expect("user message");
    insert_message(&db, chat_id, Some(request_id), "assistant", false)
        .await
        .expect("assistant message of the same request");
    let err = insert_message(&db, chat_id, Some(request_id), "user", false)
        .await
        .expect_err("second live user message of the request");
    assert_eq!(
        unique_violation_target(&err),
        Some(UniqueTarget::MessageRequestRole)
    );

    // Messages without a request id never conflict.
    insert_message(&db, chat_id, None, "system", false)
        .await
        .expect("first message without request id");
    insert_message(&db, chat_id, None, "system", false)
        .await
        .expect("second message without request id");
}

#[tokio::test]
async fn soft_deleted_message_does_not_block_its_request_id() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;
    let request_id = Uuid::new_v4();

    insert_message(&db, chat_id, Some(request_id), "user", true)
        .await
        .expect("soft-deleted user message");
    insert_message(&db, chat_id, Some(request_id), "user", false)
        .await
        .expect("new user message with the same (chat_id, request_id, role)");
}

#[tokio::test]
async fn message_defaults_are_applied() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;
    insert_message(&db, chat_id, None, "user", false)
        .await
        .expect("message");

    let row = db
        .query_one_raw(stmt(
            "SELECT content_type, request_kind, features_used, token_estimate, \
             input_tokens, is_compressed FROM messages",
        ))
        .await
        .expect("query")
        .expect("one message");
    assert_eq!(row.try_get::<String>("", "content_type").unwrap(), "text");
    assert_eq!(row.try_get::<String>("", "request_kind").unwrap(), "chat");
    assert_eq!(row.try_get::<String>("", "features_used").unwrap(), "[]");
    assert_eq!(row.try_get::<i64>("", "token_estimate").unwrap(), 0);
    assert_eq!(row.try_get::<i64>("", "input_tokens").unwrap(), 0);
    assert!(!row.try_get::<bool>("", "is_compressed").unwrap());
}

// ── attachments / message_attachments ────────────────────────────────────

#[tokio::test]
async fn message_attachment_must_reference_rows_of_the_same_chat() {
    let db = migrated_db().await;
    let chat_a = insert_chat(&db, Uuid::new_v4()).await;
    let chat_b = insert_chat(&db, Uuid::new_v4()).await;
    let message_a = insert_message(&db, chat_a, Some(Uuid::new_v4()), "user", false)
        .await
        .expect("message of chat A");
    let attachment_a = insert_attachment(&db, chat_a).await;
    let attachment_b = insert_attachment(&db, chat_b).await;

    let err = link_attachment(&db, chat_a, message_a, attachment_b)
        .await
        .expect_err("attachment of another chat");
    assert!(toolkit_db::secure::is_foreign_key_violation(&err), "{err}");

    let err = link_attachment(&db, chat_b, message_a, attachment_b)
        .await
        .expect_err("message of another chat");
    assert!(toolkit_db::secure::is_foreign_key_violation(&err), "{err}");

    link_attachment(&db, chat_a, message_a, attachment_a)
        .await
        .expect("same-chat link");
    let err = link_attachment(&db, chat_a, message_a, attachment_a)
        .await
        .expect_err("duplicate link");
    assert_eq!(
        unique_violation_target(&err),
        Some(UniqueTarget::MessageAttachment)
    );

    // Deleting the message cascades to its links.
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM messages WHERE id = ?",
        [message_a.into()],
    ))
    .await
    .expect("delete message");
    let links = db
        .query_one_raw(stmt("SELECT count(*) AS n FROM message_attachments"))
        .await
        .expect("query")
        .expect("count row");
    assert_eq!(links.try_get::<i64>("", "n").unwrap(), 0);
}

#[tokio::test]
async fn attachment_value_checks_are_enforced_but_cleanup_status_is_free() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;

    let base = || attachments::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(Uuid::new_v4()),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(Uuid::new_v4()),
        filename: Set("image.png".to_owned()),
        status: Set("pending".to_owned()),
        attachment_kind: Set("image".to_owned()),
        created_at: Set(now()),
        ..Default::default()
    };

    let mut model = base();
    model.cleanup_status = Set(Some("anything".to_owned()));
    attachments::Entity::insert(model)
        .exec_without_returning(&db)
        .await
        .expect("cleanup_status has no CHECK (ADR-0010)");

    let invalid = [
        ("status", {
            let mut model = base();
            model.status = Set("done".to_owned());
            model
        }),
        ("attachment_kind", {
            let mut model = base();
            model.attachment_kind = Set("video".to_owned());
            model
        }),
        ("secondary_status", {
            let mut model = base();
            model.secondary_status = Set("ready".to_owned());
            model
        }),
        ("secondary_provider_kind", {
            let mut model = base();
            model.secondary_provider_kind = Set(Some("openai".to_owned()));
            model
        }),
        ("cleanup_attempts", {
            let mut model = base();
            model.cleanup_attempts = Set(-1);
            model
        }),
    ];
    for (column, model) in invalid {
        attachments::Entity::insert(model)
            .exec_without_returning(&db)
            .await
            .expect_err(column);
    }

    let row = db
        .query_one_raw(stmt(
            "SELECT storage_backend, secondary_status, updated_at FROM attachments",
        ))
        .await
        .expect("query")
        .expect("one attachment");
    assert_eq!(
        row.try_get::<String>("", "storage_backend").unwrap(),
        "azure"
    );
    assert_eq!(
        row.try_get::<String>("", "secondary_status").unwrap(),
        "not_attempted"
    );
    row.try_get::<OffsetDateTime>("", "updated_at")
        .expect("updated_at default decodes as a timestamp");
    let text = row.try_get::<String>("", "updated_at").unwrap();
    assert_eq!(text.len(), "2026-01-02T03:04:05.123456Z".len(), "{text}");
    assert!(text.ends_with("000Z"), "{text}");
}

// ── timestamps ───────────────────────────────────────────────────────────

#[tokio::test]
async fn timestamps_are_stored_as_fixed_width_utc_text() {
    let db = migrated_db().await;
    let whole_second = OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap();
    let id = Uuid::new_v4();
    let model = chats::ActiveModel {
        id: Set(id),
        tenant_id: Set(Uuid::new_v4()),
        user_id: Set(Uuid::new_v4()),
        model: Set("gpt-test".to_owned()),
        title: Set(None),
        is_temporary: Set(false),
        created_at: Set(TimestampForm::FixedText.stamp(whole_second)),
        updated_at: Set(
            TimestampForm::FixedText.stamp(whole_second + time::Duration::milliseconds(500))
        ),
        deleted_at: Set(None),
    };
    chats::Entity::insert(model)
        .exec_without_returning(&db)
        .await
        .expect("insert chat");

    let row = db
        .query_one_raw(stmt("SELECT created_at, updated_at FROM chats"))
        .await
        .expect("query")
        .expect("one chat");
    assert_eq!(
        row.try_get::<String>("", "created_at").unwrap(),
        "2026-01-01T00:00:00.000000Z"
    );
    assert_eq!(
        row.try_get::<String>("", "updated_at").unwrap(),
        "2026-01-01T00:00:00.500000Z"
    );

    let stored = row.try_get::<DbTimestamp>("", "created_at").unwrap();
    assert_eq!(stored, whole_second);
    assert_eq!(stored.form(), TimestampForm::FixedText);
}

// ── message_reactions ────────────────────────────────────────────────────

#[tokio::test]
async fn reaction_check_and_uniqueness() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;
    let message_id = insert_message(&db, chat_id, None, "assistant", false)
        .await
        .expect("message");
    let user_id = Uuid::new_v4();

    insert_reaction(&db, message_id, user_id, "love")
        .await
        .expect_err("reaction CHECK");
    insert_reaction(&db, message_id, user_id, "like")
        .await
        .expect("like");
    insert_reaction(&db, message_id, Uuid::new_v4(), "dislike")
        .await
        .expect("dislike of another user");
    let err = insert_reaction(&db, message_id, user_id, "dislike")
        .await
        .expect_err("second reaction of the same user");
    assert_eq!(
        unique_violation_target(&err),
        Some(UniqueTarget::MessageReaction)
    );
}

// ── Other tables ─────────────────────────────────────────────────────────

#[tokio::test]
async fn one_thread_summary_and_one_vector_store_per_chat() {
    let db = migrated_db().await;
    let tenant_id = Uuid::new_v4();
    let chat_id = insert_chat(&db, tenant_id).await;

    let summary = || thread_summaries::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        summary_text: Set("summary".to_owned()),
        summarized_up_to_created_at: Set(now()),
        summarized_up_to_message_id: Set(Uuid::new_v4()),
        token_estimate: Set(10),
        created_at: Set(now()),
        updated_at: Set(now()),
    };
    thread_summaries::Entity::insert(summary())
        .exec_without_returning(&db)
        .await
        .expect("summary");
    let err = thread_summaries::Entity::insert(summary())
        .exec_without_returning(&db)
        .await
        .expect_err("second summary");
    assert_eq!(
        unique_violation_target(&err),
        Some(UniqueTarget::ThreadSummaryChat)
    );

    let store = |file_count: i32| chat_vector_stores::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        vector_store_id: Set(None),
        provider: Set("azure".to_owned()),
        file_count: Set(file_count),
        created_at: Set(now()),
    };
    chat_vector_stores::Entity::insert(store(-1))
        .exec_without_returning(&db)
        .await
        .expect_err("file_count CHECK");
    chat_vector_stores::Entity::insert(store(0))
        .exec_without_returning(&db)
        .await
        .expect("vector store placeholder");
    let err = chat_vector_stores::Entity::insert(store(0))
        .exec_without_returning(&db)
        .await
        .expect_err("second vector store of the chat");
    assert_eq!(
        unique_violation_target(&err),
        Some(UniqueTarget::ChatVectorStore)
    );
}

#[tokio::test]
async fn quota_usage_has_one_row_per_bucket() {
    let db = migrated_db().await;
    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let period_start = time::Date::from_calendar_date(2026, time::Month::October, 1).unwrap();

    let row = |bucket: &str, reserved: i64| quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        user_id: Set(user_id),
        period_type: Set("monthly".to_owned()),
        period_start: Set(period_start),
        bucket: Set(bucket.to_owned()),
        reserved_credits_micro: Set(reserved),
        updated_at: Set(now()),
        ..Default::default()
    };
    quota_usage::Entity::insert(row("total", 5))
        .exec_without_returning(&db)
        .await
        .expect("total bucket");
    quota_usage::Entity::insert(row("tier:premium", 5))
        .exec_without_returning(&db)
        .await
        .expect("premium bucket");
    quota_usage::Entity::insert(row("tier:standard", -1))
        .exec_without_returning(&db)
        .await
        .expect_err("non-negative counter CHECK");
    let err = quota_usage::Entity::insert(row("total", 1))
        .exec_without_returning(&db)
        .await
        .expect_err("second total bucket of the period");
    assert_eq!(
        unique_violation_target(&err),
        Some(UniqueTarget::QuotaUsageBucket)
    );

    let stored = db
        .query_one_raw(stmt(
            "SELECT period_start FROM quota_usage WHERE bucket = 'total'",
        ))
        .await
        .expect("query")
        .expect("total row");
    assert_eq!(
        stored.try_get::<time::Date>("", "period_start").unwrap(),
        period_start
    );
}

// ── Statement lists ──────────────────────────────────────────────────────

#[test]
fn every_named_unique_target_is_declared_on_both_backends() {
    let targets = [
        UniqueTarget::RunningTurn,
        UniqueTarget::TurnRequestId,
        UniqueTarget::MessageRequestRole,
        UniqueTarget::ThreadSummaryChat,
        UniqueTarget::ChatVectorStore,
        UniqueTarget::QuotaUsageBucket,
        UniqueTarget::MessageReaction,
        UniqueTarget::MessageAttachment,
        UniqueTarget::PrimaryKey(DbTable::Messages),
        UniqueTarget::PrimaryKey(DbTable::Attachments),
    ];
    for backend in [DbBackend::Postgres, DbBackend::Sqlite] {
        let sql = all_statements(backend).join("\n");
        for target in targets {
            let name = target.constraint_name().expect("named target");
            assert!(sql.contains(name), "{backend:?} does not declare {name}");
        }
    }
}

#[test]
fn statement_lists_follow_the_design_constraints() {
    let postgres = all_statements(DbBackend::Postgres).join("\n");
    let sqlite = all_statements(DbBackend::Sqlite).join("\n");

    for sql in [&postgres, &sqlite] {
        for table in GEAR_TABLES {
            assert!(
                sql.contains(&format!("CREATE TABLE IF NOT EXISTS {table} (")),
                "{table} is not created"
            );
        }
        assert!(!sql.contains("mcp"), "no MCP table");
        assert!(
            !sql.contains("completed_at IS NOT NULL"),
            "no cross-column CHECK"
        );
        assert!(
            !sql.contains("last_progress_at IS NOT NULL"),
            "no cross-column CHECK"
        );
        assert!(
            !sql.contains("cleanup_status IN"),
            "no cleanup_status CHECK"
        );
        assert!(sql.contains("idx_attachments_stale_upload"));
        assert!(sql.contains("last_progress_at"));
    }

    assert!(postgres.contains("JSONB") && postgres.contains("TIMESTAMPTZ"));
    assert!(postgres.contains("BYTEA") && postgres.contains("DATE NOT NULL"));
    for pg_only in ["UUID", "TIMESTAMPTZ", "JSONB", "BYTEA", "now()"] {
        assert!(!sqlite.contains(pg_only), "SQLite statements use {pg_only}");
    }
}

// ── Error mapping ────────────────────────────────────────────────────────

fn exec_err(message: &str) -> DbErr {
    DbErr::Exec(RuntimeErr::Internal(message.to_owned()))
}

#[test]
fn postgres_unique_violations_are_identified_by_constraint_name() {
    let cases = [
        ("uq_chat_turns_running", UniqueTarget::RunningTurn),
        ("uq_chat_turns_chat_request", UniqueTarget::TurnRequestId),
        (
            "uq_messages_chat_request_role",
            UniqueTarget::MessageRequestRole,
        ),
        ("uq_quota_usage_bucket", UniqueTarget::QuotaUsageBucket),
        (
            "chat_turns_pkey",
            UniqueTarget::PrimaryKey(DbTable::ChatTurns),
        ),
        ("something_else", UniqueTarget::Unknown),
    ];
    for (name, expected) in cases {
        let err = exec_err(&format!(
            "error returned from database: duplicate key value violates unique constraint \"{name}\""
        ));
        assert_eq!(unique_violation_target(&err), Some(expected), "{name}");
    }

    let fk = exec_err(
        "error returned from database: insert or update on table \"message_attachments\" \
         violates foreign key constraint \"fk_message_attachments_attachment\"",
    );
    assert_eq!(unique_violation_target(&fk), None);
}

#[test]
fn sqlite_unique_violations_are_identified_by_column_list() {
    let cases = [
        ("chat_turns.chat_id", UniqueTarget::RunningTurn),
        (
            "chat_turns.chat_id, chat_turns.request_id",
            UniqueTarget::TurnRequestId,
        ),
        (
            "messages.chat_id, messages.request_id, messages.role",
            UniqueTarget::MessageRequestRole,
        ),
        ("chats.id", UniqueTarget::PrimaryKey(DbTable::Chats)),
        ("index 'uq_chat_turns_running'", UniqueTarget::RunningTurn),
        ("unknown_table.x", UniqueTarget::Unknown),
    ];
    for (columns, expected) in cases {
        let err = exec_err(&format!(
            "error returned from database: (code: 2067) UNIQUE constraint failed: {columns}"
        ));
        assert_eq!(unique_violation_target(&err), Some(expected), "{columns}");
    }
}

#[tokio::test]
async fn db_errors_map_to_domain_errors_without_driver_text() {
    let db = migrated_db().await;
    let chat_id = insert_chat(&db, Uuid::new_v4()).await;
    insert_turn(&db, chat_id, Uuid::new_v4(), "running", false)
        .await
        .expect("running turn");
    let unique = insert_turn(&db, chat_id, Uuid::new_v4(), "running", false)
        .await
        .expect_err("second running turn");

    let mapped = DomainError::from(DbError::Sea(unique));
    assert!(
        matches!(
            mapped,
            DomainError::AlreadyExists {
                resource: ResourceKind::Turn,
                resource_name: conflict::UNIQUE_VIOLATION,
            }
        ),
        "{mapped:?}"
    );

    let fk = link_attachment(&db, chat_id, Uuid::new_v4(), Uuid::new_v4())
        .await
        .expect_err("dangling link");
    match DomainError::from(DbError::Sea(fk)) {
        DomainError::Internal(diagnostic) => {
            assert!(
                !diagnostic.to_lowercase().contains("constraint"),
                "{diagnostic}"
            );
            assert!(!diagnostic.contains("message_attachments"), "{diagnostic}");
        }
        other => panic!("expected internal, got {other:?}"),
    }
}
