//! Schema migrations of the gear's P1 tables (DESIGN section 3.7 "Database
//! Schemas & Tables").
//!
//! One migration per table, in foreign-key order: `chats`, `messages`,
//! `chat_turns`, `attachments`, `message_attachments`, `thread_summaries`,
//! `chat_vector_stores`, `quota_usage`, `message_reactions`. No migration
//! creates the deferred MCP tables (ADR-0006).
//!
//! Every migration runs on PostgreSQL and `SQLite`. The DDL is raw SQL so that
//! the partial indexes, CHECKs and composite foreign keys are kept verbatim;
//! where the dialects differ the table has a PostgreSQL and a `SQLite`
//! variant, the index statements are shared. Type lowering on `SQLite`:
//! `UUID` -> `TEXT` (the driver writes the 16 UUID bytes as a `BLOB` value),
//! `TIMESTAMPTZ` / `DATE` -> `TEXT`, `JSONB` -> `TEXT`, `BYTEA` -> `BLOB`.
//! `SQLite` timestamp text is always the fixed-width UTC form
//! `YYYY-MM-DDTHH:MM:SS.ffffffZ` (`super::entities::fixed_text`), so byte
//! order is chronological; the `attachments.updated_at` default produces the
//! same form (`strftime` has millisecond precision, padded to six digits).
//!
//! Enforced CHECKs are the simple value sets (`state`, `requester_type`,
//! `attachment_kind`, `status`, `secondary_status`, `secondary_provider_kind`,
//! `reaction`) and the non-negative counters. The cross-column `chat_turns`
//! CHECKs and the `attachments.cleanup_status` value set are invariants kept by
//! the repositories, not by the database (ADR-0010).
//!
//! Applied migrations are never edited: a later schema change is a new
//! column migration ([`ColumnMigration`]) whose statements run on both
//! backends (`attachments.secondary_provider_id`, the provider entry the
//! secondary copy was uploaded with).
//!
//! The shared outbox tables come from the platform outbox migrations;
//! [`migrations`] appends them to the gear's own set.
//!
//! Unique constraint and index names are part of the persistence contract:
//! `super::error::UniqueTarget` maps a unique violation back to them.

use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

const MYSQL_NOT_SUPPORTED: &str =
    "mini-chat migrations: MySQL is not supported (the schema targets PostgreSQL and SQLite)";

/// The gear's schema migrator (gear tables only; see [`migrations`] for the
/// full set including the outbox tables).
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        TABLE_MIGRATIONS
            .iter()
            .map(|migration| Box::new(*migration) as Box<dyn MigrationTrait>)
            .chain(
                COLUMN_MIGRATIONS
                    .iter()
                    .map(|migration| Box::new(*migration) as Box<dyn MigrationTrait>),
            )
            .collect()
    }
}

/// Every migration of the gear: the P1 tables of [`Migrator`] followed by the
/// shared `toolkit-db` outbox migrations (DESIGN section 3.2 "Gear lifecycle":
/// gear migrations include the outbox migrations).
#[must_use]
pub fn migrations() -> Vec<Box<dyn MigrationTrait>> {
    let mut all = Migrator::migrations();
    all.extend(toolkit_db::outbox::outbox_migrations());
    all
}

/// A table migration: its dialect-specific statements and the table it drops
/// on `down`.
#[derive(Clone, Copy)]
struct TableMigration {
    name: &'static str,
    table: &'static str,
    postgres: &'static [&'static str],
    sqlite: &'static [&'static str],
}

impl MigrationName for TableMigration {
    fn name(&self) -> &str {
        self.name
    }
}

#[async_trait::async_trait]
impl MigrationTrait for TableMigration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let statements = match manager.get_database_backend() {
            DatabaseBackend::Postgres => self.postgres,
            DatabaseBackend::Sqlite => self.sqlite,
            _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        };
        let conn = manager.get_connection();
        for sql in statements {
            conn.execute_unprepared(sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        match manager.get_database_backend() {
            DatabaseBackend::Postgres | DatabaseBackend::Sqlite => {}
            _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        }
        manager
            .get_connection()
            .execute_unprepared(&format!("DROP TABLE IF EXISTS {}", self.table))
            .await?;
        Ok(())
    }
}

/// A column migration on an existing table: the same statements on both
/// backends (`up`), reverted by `down`.
#[derive(Clone, Copy)]
struct ColumnMigration {
    name: &'static str,
    up: &'static [&'static str],
    down: &'static [&'static str],
}

impl MigrationName for ColumnMigration {
    fn name(&self) -> &str {
        self.name
    }
}

#[async_trait::async_trait]
impl MigrationTrait for ColumnMigration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        run_on_supported_backend(manager, self.up).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        run_on_supported_backend(manager, self.down).await
    }
}

/// Runs `statements` on PostgreSQL or `SQLite`; MySQL is rejected.
async fn run_on_supported_backend(
    manager: &SchemaManager<'_>,
    statements: &[&str],
) -> Result<(), DbErr> {
    match manager.get_database_backend() {
        DatabaseBackend::Postgres | DatabaseBackend::Sqlite => {}
        _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
    }
    let conn = manager.get_connection();
    for sql in statements {
        conn.execute_unprepared(sql).await?;
    }
    Ok(())
}

/// The gear's table migrations in foreign-key order (the migration runner
/// sorts by name, so the names keep the same order).
const TABLE_MIGRATIONS: &[TableMigration] = &[
    TableMigration {
        name: "m20261011_000001_create_chats",
        table: "chats",
        postgres: &[PG_CHATS, IDX_CHATS_LIST],
        sqlite: &[SQLITE_CHATS, IDX_CHATS_LIST],
    },
    TableMigration {
        name: "m20261011_000002_create_messages",
        table: "messages",
        postgres: &[
            PG_MESSAGES,
            UQ_MESSAGES_REQUEST_ROLE,
            IDX_MESSAGES_CHAT_CREATED,
        ],
        sqlite: &[
            SQLITE_MESSAGES,
            UQ_MESSAGES_REQUEST_ROLE,
            IDX_MESSAGES_CHAT_CREATED,
        ],
    },
    TableMigration {
        name: "m20261011_000003_create_chat_turns",
        table: "chat_turns",
        postgres: &[
            PG_CHAT_TURNS,
            IDX_CHAT_TURNS_CHAT_STARTED,
            UQ_CHAT_TURNS_RUNNING,
            IDX_CHAT_TURNS_ORPHAN_SCAN,
        ],
        sqlite: &[
            SQLITE_CHAT_TURNS,
            IDX_CHAT_TURNS_CHAT_STARTED,
            UQ_CHAT_TURNS_RUNNING,
            IDX_CHAT_TURNS_ORPHAN_SCAN,
        ],
    },
    TableMigration {
        name: "m20261011_000004_create_attachments",
        table: "attachments",
        postgres: &[
            PG_ATTACHMENTS,
            IDX_ATTACHMENTS_TENANT_CHAT,
            IDX_ATTACHMENTS_CLEANUP_STATUS,
            IDX_ATTACHMENTS_STALE_UPLOAD,
        ],
        sqlite: &[
            SQLITE_ATTACHMENTS,
            IDX_ATTACHMENTS_TENANT_CHAT,
            IDX_ATTACHMENTS_CLEANUP_STATUS,
            IDX_ATTACHMENTS_STALE_UPLOAD,
        ],
    },
    TableMigration {
        name: "m20261011_000005_create_message_attachments",
        table: "message_attachments",
        postgres: &[
            PG_MESSAGE_ATTACHMENTS,
            IDX_MESSAGE_ATTACHMENTS_TENANT_CHAT,
            IDX_MESSAGE_ATTACHMENTS_ATTACHMENT_CHAT,
        ],
        sqlite: &[
            SQLITE_MESSAGE_ATTACHMENTS,
            IDX_MESSAGE_ATTACHMENTS_TENANT_CHAT,
            IDX_MESSAGE_ATTACHMENTS_ATTACHMENT_CHAT,
        ],
    },
    TableMigration {
        name: "m20261011_000006_create_thread_summaries",
        table: "thread_summaries",
        postgres: &[PG_THREAD_SUMMARIES],
        sqlite: &[SQLITE_THREAD_SUMMARIES],
    },
    TableMigration {
        name: "m20261011_000007_create_chat_vector_stores",
        table: "chat_vector_stores",
        postgres: &[PG_CHAT_VECTOR_STORES],
        sqlite: &[SQLITE_CHAT_VECTOR_STORES],
    },
    TableMigration {
        name: "m20261011_000008_create_quota_usage",
        table: "quota_usage",
        postgres: &[PG_QUOTA_USAGE],
        sqlite: &[SQLITE_QUOTA_USAGE],
    },
    TableMigration {
        name: "m20261011_000009_create_message_reactions",
        table: "message_reactions",
        postgres: &[PG_MESSAGE_REACTIONS],
        sqlite: &[SQLITE_MESSAGE_REACTIONS],
    },
];

/// Column migrations applied after the table migrations (names sort after
/// them).
const COLUMN_MIGRATIONS: &[ColumnMigration] = &[ColumnMigration {
    name: "m20261011_000010_add_attachments_secondary_provider_id",
    up: &[ADD_ATTACHMENTS_SECONDARY_PROVIDER_ID],
    down: &[DROP_ATTACHMENTS_SECONDARY_PROVIDER_ID],
}];

/// The provider entry the secondary copy was uploaded with, so that its
/// delete resolves the same upstream (nullable: rows without a copy, and
/// rows written before this migration).
const ADD_ATTACHMENTS_SECONDARY_PROVIDER_ID: &str =
    "ALTER TABLE attachments ADD COLUMN secondary_provider_id VARCHAR(128)";

const DROP_ATTACHMENTS_SECONDARY_PROVIDER_ID: &str =
    "ALTER TABLE attachments DROP COLUMN secondary_provider_id";

// ── chats ────────────────────────────────────────────────────────────────

const PG_CHATS: &str = r"
CREATE TABLE IF NOT EXISTS chats (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    user_id UUID NOT NULL,
    model VARCHAR(1024) NOT NULL,
    title VARCHAR(255),
    is_temporary BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    deleted_at TIMESTAMPTZ
)";

const SQLITE_CHATS: &str = r"
CREATE TABLE IF NOT EXISTS chats (
    id TEXT PRIMARY KEY NOT NULL,
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    model VARCHAR(1024) NOT NULL,
    title VARCHAR(255),
    is_temporary BOOLEAN NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    deleted_at TEXT
)";

const IDX_CHATS_LIST: &str = "CREATE INDEX IF NOT EXISTS idx_chats_tenant_user_updated \
     ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL";

// ── messages ─────────────────────────────────────────────────────────────

const PG_MESSAGES: &str = r"
CREATE TABLE IF NOT EXISTS messages (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id UUID,
    role VARCHAR(16) NOT NULL,
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL DEFAULT 'text',
    token_estimate INTEGER NOT NULL DEFAULT 0 CHECK (token_estimate >= 0),
    provider_response_id VARCHAR(128),
    request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
    features_used JSONB NOT NULL DEFAULT '[]'::jsonb,
    input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens BIGINT NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cache_read_input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cache_read_input_tokens >= 0),
    cache_write_input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cache_write_input_tokens >= 0),
    reasoning_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reasoning_tokens >= 0),
    model VARCHAR(1024),
    is_compressed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL,
    deleted_at TIMESTAMPTZ,
    CONSTRAINT uq_messages_id_chat UNIQUE (id, chat_id)
)";

const SQLITE_MESSAGES: &str = r"
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY NOT NULL,
    tenant_id TEXT NOT NULL,
    chat_id TEXT NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id TEXT,
    role VARCHAR(16) NOT NULL,
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL DEFAULT 'text',
    token_estimate INTEGER NOT NULL DEFAULT 0 CHECK (token_estimate >= 0),
    provider_response_id VARCHAR(128),
    request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
    features_used TEXT NOT NULL DEFAULT '[]',
    input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens BIGINT NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cache_read_input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cache_read_input_tokens >= 0),
    cache_write_input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cache_write_input_tokens >= 0),
    reasoning_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reasoning_tokens >= 0),
    model VARCHAR(1024),
    is_compressed BOOLEAN NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    deleted_at TEXT,
    CONSTRAINT uq_messages_id_chat UNIQUE (id, chat_id)
)";

/// One user and one assistant message per live `request_id`.
const UQ_MESSAGES_REQUEST_ROLE: &str = "CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role \
     ON messages (chat_id, request_id, role) \
     WHERE request_id IS NOT NULL AND deleted_at IS NULL";

const IDX_MESSAGES_CHAT_CREATED: &str = "CREATE INDEX IF NOT EXISTS idx_messages_chat_created \
     ON messages (chat_id, created_at) WHERE deleted_at IS NULL";

// ── chat_turns ───────────────────────────────────────────────────────────

const PG_CHAT_TURNS: &str = r"
CREATE TABLE IF NOT EXISTS chat_turns (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id UUID NOT NULL,
    requester_type VARCHAR(16) NOT NULL CHECK (requester_type IN ('user', 'system')),
    requester_user_id UUID,
    state VARCHAR(16) NOT NULL CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
    provider_name VARCHAR(128),
    provider_response_id VARCHAR(128),
    assistant_message_id UUID,
    error_code VARCHAR(64),
    reserve_tokens BIGINT,
    max_output_tokens_applied INTEGER,
    reserved_credits_micro BIGINT,
    policy_version_applied BIGINT,
    effective_model VARCHAR(1024),
    minimal_generation_floor_applied INTEGER,
    error_detail TEXT,
    deleted_at TIMESTAMPTZ,
    replaced_by_request_id UUID,
    started_at TIMESTAMPTZ NOT NULL,
    last_progress_at TIMESTAMPTZ,
    web_search_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    web_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (web_search_completed_count >= 0),
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (code_interpreter_completed_count >= 0),
    file_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (file_search_completed_count >= 0),
    completed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT uq_chat_turns_chat_request UNIQUE (chat_id, request_id)
)";

const SQLITE_CHAT_TURNS: &str = r"
CREATE TABLE IF NOT EXISTS chat_turns (
    id TEXT PRIMARY KEY NOT NULL,
    tenant_id TEXT NOT NULL,
    chat_id TEXT NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id TEXT NOT NULL,
    requester_type VARCHAR(16) NOT NULL CHECK (requester_type IN ('user', 'system')),
    requester_user_id TEXT,
    state VARCHAR(16) NOT NULL CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
    provider_name VARCHAR(128),
    provider_response_id VARCHAR(128),
    assistant_message_id TEXT,
    error_code VARCHAR(64),
    reserve_tokens BIGINT,
    max_output_tokens_applied INTEGER,
    reserved_credits_micro BIGINT,
    policy_version_applied BIGINT,
    effective_model VARCHAR(1024),
    minimal_generation_floor_applied INTEGER,
    error_detail TEXT,
    deleted_at TEXT,
    replaced_by_request_id TEXT,
    started_at TEXT NOT NULL,
    last_progress_at TEXT,
    web_search_enabled BOOLEAN NOT NULL DEFAULT 0,
    web_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (web_search_completed_count >= 0),
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (code_interpreter_completed_count >= 0),
    file_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (file_search_completed_count >= 0),
    completed_at TEXT,
    updated_at TEXT NOT NULL,
    CONSTRAINT uq_chat_turns_chat_request UNIQUE (chat_id, request_id)
)";

const IDX_CHAT_TURNS_CHAT_STARTED: &str = "CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started \
     ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL";

/// At most one live running turn per chat.
const UQ_CHAT_TURNS_RUNNING: &str = "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_running \
     ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL";

const IDX_CHAT_TURNS_ORPHAN_SCAN: &str = "CREATE INDEX IF NOT EXISTS idx_chat_turns_orphan_scan \
     ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL";

// ── attachments ──────────────────────────────────────────────────────────

const PG_ATTACHMENTS: &str = r"
CREATE TABLE IF NOT EXISTS attachments (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    uploaded_by_user_id UUID NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128),
    size_bytes BIGINT,
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128),
    status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    error_code VARCHAR(64),
    attachment_kind VARCHAR(16) NOT NULL CHECK (attachment_kind IN ('document', 'image')),
    for_file_search BOOLEAN NOT NULL DEFAULT FALSE,
    for_code_interpreter BOOLEAN NOT NULL DEFAULT FALSE,
    doc_summary TEXT,
    img_thumbnail BYTEA,
    img_thumbnail_width INTEGER,
    img_thumbnail_height INTEGER,
    summary_model VARCHAR(1024),
    summary_updated_at TIMESTAMPTZ,
    cleanup_status VARCHAR(16),
    cleanup_attempts INTEGER NOT NULL DEFAULT 0 CHECK (cleanup_attempts >= 0),
    last_cleanup_error TEXT,
    cleanup_updated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ,
    secondary_file_id VARCHAR(128),
    secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted'
        CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
    secondary_provider_kind VARCHAR(32)
        CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic')),
    CONSTRAINT uq_attachments_id_chat UNIQUE (id, chat_id)
)";

const SQLITE_ATTACHMENTS: &str = r"
CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY NOT NULL,
    tenant_id TEXT NOT NULL,
    chat_id TEXT NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    uploaded_by_user_id TEXT NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128),
    size_bytes BIGINT,
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128),
    status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    error_code VARCHAR(64),
    attachment_kind VARCHAR(16) NOT NULL CHECK (attachment_kind IN ('document', 'image')),
    for_file_search BOOLEAN NOT NULL DEFAULT 0,
    for_code_interpreter BOOLEAN NOT NULL DEFAULT 0,
    doc_summary TEXT,
    img_thumbnail BLOB,
    img_thumbnail_width INTEGER,
    img_thumbnail_height INTEGER,
    summary_model VARCHAR(1024),
    summary_updated_at TEXT,
    cleanup_status VARCHAR(16),
    cleanup_attempts INTEGER NOT NULL DEFAULT 0 CHECK (cleanup_attempts >= 0),
    last_cleanup_error TEXT,
    cleanup_updated_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now') || '000Z'),
    deleted_at TEXT,
    secondary_file_id VARCHAR(128),
    secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted'
        CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
    secondary_provider_kind VARCHAR(32)
        CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic')),
    CONSTRAINT uq_attachments_id_chat UNIQUE (id, chat_id)
)";

const IDX_ATTACHMENTS_TENANT_CHAT: &str = "CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat \
     ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL";

const IDX_ATTACHMENTS_CLEANUP_STATUS: &str = "CREATE INDEX IF NOT EXISTS idx_attachments_cleanup_status \
     ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL";

/// Upload reaper scan; a plain index because the scan binds the status
/// values as parameters (a `SQLite` partial index needs a literal `WHERE`).
const IDX_ATTACHMENTS_STALE_UPLOAD: &str = "CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload \
     ON attachments (status, cleanup_status, deleted_at, updated_at)";

// ── message_attachments ──────────────────────────────────────────────────

const PG_MESSAGE_ATTACHMENTS: &str = r"
CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL,
    message_id UUID NOT NULL,
    attachment_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT pk_message_attachments PRIMARY KEY (chat_id, message_id, attachment_id),
    CONSTRAINT fk_message_attachments_message FOREIGN KEY (message_id, chat_id)
        REFERENCES messages (id, chat_id) ON DELETE CASCADE,
    CONSTRAINT fk_message_attachments_attachment FOREIGN KEY (attachment_id, chat_id)
        REFERENCES attachments (id, chat_id) ON DELETE CASCADE
)";

const SQLITE_MESSAGE_ATTACHMENTS: &str = r"
CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    attachment_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    CONSTRAINT pk_message_attachments PRIMARY KEY (chat_id, message_id, attachment_id),
    CONSTRAINT fk_message_attachments_message FOREIGN KEY (message_id, chat_id)
        REFERENCES messages (id, chat_id) ON DELETE CASCADE,
    CONSTRAINT fk_message_attachments_attachment FOREIGN KEY (attachment_id, chat_id)
        REFERENCES attachments (id, chat_id) ON DELETE CASCADE
)";

const IDX_MESSAGE_ATTACHMENTS_TENANT_CHAT: &str = "CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat \
     ON message_attachments (tenant_id, chat_id)";

const IDX_MESSAGE_ATTACHMENTS_ATTACHMENT_CHAT: &str = "CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment_chat \
     ON message_attachments (attachment_id, chat_id)";

// ── thread_summaries ─────────────────────────────────────────────────────

const PG_THREAD_SUMMARIES: &str = r"
CREATE TABLE IF NOT EXISTS thread_summaries (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at TIMESTAMPTZ NOT NULL,
    summarized_up_to_message_id UUID NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0 CHECK (token_estimate >= 0),
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT uq_thread_summaries_chat UNIQUE (chat_id)
)";

const SQLITE_THREAD_SUMMARIES: &str = r"
CREATE TABLE IF NOT EXISTS thread_summaries (
    id TEXT PRIMARY KEY NOT NULL,
    tenant_id TEXT NOT NULL,
    chat_id TEXT NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at TEXT NOT NULL,
    summarized_up_to_message_id TEXT NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0 CHECK (token_estimate >= 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CONSTRAINT uq_thread_summaries_chat UNIQUE (chat_id)
)";

// ── chat_vector_stores ───────────────────────────────────────────────────

const PG_CHAT_VECTOR_STORES: &str = r"
CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL,
    vector_store_id VARCHAR(128),
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT uq_chat_vector_stores_tenant_chat UNIQUE (tenant_id, chat_id)
)";

const SQLITE_CHAT_VECTOR_STORES: &str = r"
CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id TEXT PRIMARY KEY NOT NULL,
    tenant_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    vector_store_id VARCHAR(128),
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at TEXT NOT NULL,
    CONSTRAINT uq_chat_vector_stores_tenant_chat UNIQUE (tenant_id, chat_id)
)";

// ── quota_usage ──────────────────────────────────────────────────────────
//
// The index of `uq_quota_usage_bucket` is also the quota lookup index on
// `(tenant_id, user_id, period_type, period_start, bucket)`.

const PG_QUOTA_USAGE: &str = r"
CREATE TABLE IF NOT EXISTS quota_usage (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    user_id UUID NOT NULL,
    period_type VARCHAR(16) NOT NULL,
    period_start DATE NOT NULL,
    bucket VARCHAR(32) NOT NULL,
    spent_credits_micro BIGINT NOT NULL DEFAULT 0 CHECK (spent_credits_micro >= 0),
    reserved_credits_micro BIGINT NOT NULL DEFAULT 0 CHECK (reserved_credits_micro >= 0),
    calls INTEGER NOT NULL DEFAULT 0 CHECK (calls >= 0),
    input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens BIGINT NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    file_search_calls INTEGER NOT NULL DEFAULT 0 CHECK (file_search_calls >= 0),
    web_search_calls INTEGER NOT NULL DEFAULT 0 CHECK (web_search_calls >= 0),
    code_interpreter_calls INTEGER NOT NULL DEFAULT 0 CHECK (code_interpreter_calls >= 0),
    rag_retrieval_calls INTEGER NOT NULL DEFAULT 0 CHECK (rag_retrieval_calls >= 0),
    image_inputs INTEGER NOT NULL DEFAULT 0 CHECK (image_inputs >= 0),
    image_upload_bytes BIGINT NOT NULL DEFAULT 0 CHECK (image_upload_bytes >= 0),
    updated_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT uq_quota_usage_bucket UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
)";

const SQLITE_QUOTA_USAGE: &str = r"
CREATE TABLE IF NOT EXISTS quota_usage (
    id TEXT PRIMARY KEY NOT NULL,
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    period_type VARCHAR(16) NOT NULL,
    period_start TEXT NOT NULL,
    bucket VARCHAR(32) NOT NULL,
    spent_credits_micro BIGINT NOT NULL DEFAULT 0 CHECK (spent_credits_micro >= 0),
    reserved_credits_micro BIGINT NOT NULL DEFAULT 0 CHECK (reserved_credits_micro >= 0),
    calls INTEGER NOT NULL DEFAULT 0 CHECK (calls >= 0),
    input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens BIGINT NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    file_search_calls INTEGER NOT NULL DEFAULT 0 CHECK (file_search_calls >= 0),
    web_search_calls INTEGER NOT NULL DEFAULT 0 CHECK (web_search_calls >= 0),
    code_interpreter_calls INTEGER NOT NULL DEFAULT 0 CHECK (code_interpreter_calls >= 0),
    rag_retrieval_calls INTEGER NOT NULL DEFAULT 0 CHECK (rag_retrieval_calls >= 0),
    image_inputs INTEGER NOT NULL DEFAULT 0 CHECK (image_inputs >= 0),
    image_upload_bytes BIGINT NOT NULL DEFAULT 0 CHECK (image_upload_bytes >= 0),
    updated_at TEXT NOT NULL,
    CONSTRAINT uq_quota_usage_bucket UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
)";

// ── message_reactions ────────────────────────────────────────────────────
//
// The index of `uq_message_reactions_message_user` also serves lookups by
// message.

const PG_MESSAGE_REACTIONS: &str = r"
CREATE TABLE IF NOT EXISTS message_reactions (
    id UUID PRIMARY KEY,
    message_id UUID NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id UUID NOT NULL,
    tenant_id UUID NOT NULL,
    reaction VARCHAR(16) NOT NULL CHECK (reaction IN ('like', 'dislike')),
    created_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT uq_message_reactions_message_user UNIQUE (message_id, user_id)
)";

const SQLITE_MESSAGE_REACTIONS: &str = r"
CREATE TABLE IF NOT EXISTS message_reactions (
    id TEXT PRIMARY KEY NOT NULL,
    message_id TEXT NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    reaction VARCHAR(16) NOT NULL CHECK (reaction IN ('like', 'dislike')),
    created_at TEXT NOT NULL,
    CONSTRAINT uq_message_reactions_message_user UNIQUE (message_id, user_id)
)";

/// Every statement of every table migration on `backend` (test support for
/// the statement-list checks).
#[cfg(test)]
pub(super) fn all_statements(backend: DatabaseBackend) -> Vec<&'static str> {
    TABLE_MIGRATIONS
        .iter()
        .flat_map(|migration| match backend {
            DatabaseBackend::Postgres => migration.postgres.iter().copied(),
            _ => migration.sqlite.iter().copied(),
        })
        .chain(
            COLUMN_MIGRATIONS
                .iter()
                .flat_map(|migration| migration.up.iter().copied()),
        )
        .collect()
}
