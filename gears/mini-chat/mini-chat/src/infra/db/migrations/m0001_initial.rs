//! Initial mini-chat schema: the nine P1 tables of DESIGN section 3.7 on
//! `PostgreSQL` and `SQLite`.
//!
//! Raw SQL is used (not the `SeaORM` schema builder) so that partial and
//! composite unique indexes, CHECKs and composite foreign keys are preserved
//! verbatim. One statement template serves both engines; the few type
//! differences are substituted from [`Dialect`]. Simple value CHECKs are
//! enforced by the database; the cross-column `chat_turns` CHECKs and the
//! `attachments.cleanup_status` value set are kept by the repositories
//! (`cpt-cf-mini-chat-adr-runtime-consistency-limitations`).
//!
//! No `mcp_servers`, `mcp_server_tools`, `role_mcp_servers` or `tenant_closure`
//! table is created.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-schema:p1

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend};

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Column types of one engine.
///
/// `SQLite` declares UUID columns `TEXT` (their values are written as 16-byte
/// `BLOB`s by the driver) and maps `TIMESTAMPTZ`, `JSONB`, `BYTEA` and `DATE` to
/// `TEXT` / `BLOB`; `BOOLEAN` and the integer types agree on both engines.
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-sqlite-types
struct Dialect {
    uuid: &'static str,
    timestamp: &'static str,
    json: &'static str,
    bytes: &'static str,
    date: &'static str,
}

const POSTGRES: Dialect = Dialect {
    uuid: "UUID",
    timestamp: "TIMESTAMPTZ",
    json: "JSONB",
    bytes: "BYTEA",
    date: "DATE",
};

const SQLITE: Dialect = Dialect {
    uuid: "TEXT",
    timestamp: "TEXT",
    json: "TEXT",
    bytes: "BLOB",
    date: "TEXT",
};
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-sqlite-types

impl Dialect {
    fn render(&self, template: &str) -> String {
        template
            .replace("@UUID@", self.uuid)
            .replace("@TS@", self.timestamp)
            .replace("@JSON@", self.json)
            .replace("@BYTES@", self.bytes)
            .replace("@DATE@", self.date)
    }
}

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-create-tables
const CHATS: &str = r"
CREATE TABLE IF NOT EXISTS chats (
    id @UUID@ PRIMARY KEY NOT NULL,
    tenant_id @UUID@ NOT NULL,
    user_id @UUID@ NOT NULL,
    model VARCHAR(1024) NOT NULL,
    title VARCHAR(255) NULL,
    is_temporary BOOLEAN NOT NULL DEFAULT false,
    created_at @TS@ NOT NULL,
    updated_at @TS@ NOT NULL,
    deleted_at @TS@ NULL
)";

const MESSAGES: &str = r"
CREATE TABLE IF NOT EXISTS messages (
    id @UUID@ PRIMARY KEY NOT NULL,
    tenant_id @UUID@ NOT NULL,
    chat_id @UUID@ NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id @UUID@ NULL,
    role VARCHAR(16) NOT NULL,
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    provider_response_id VARCHAR(128) NULL,
    request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
    features_used @JSON@ NOT NULL DEFAULT '[]',
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cache_read_input_tokens BIGINT NOT NULL DEFAULT 0,
    cache_write_input_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    model VARCHAR(1024) NULL,
    is_compressed BOOLEAN NOT NULL DEFAULT false,
    created_at @TS@ NOT NULL,
    deleted_at @TS@ NULL,
    UNIQUE (id, chat_id)
)";

const CHAT_TURNS: &str = r"
CREATE TABLE IF NOT EXISTS chat_turns (
    id @UUID@ PRIMARY KEY NOT NULL,
    tenant_id @UUID@ NOT NULL,
    chat_id @UUID@ NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id @UUID@ NOT NULL,
    requester_type VARCHAR(16) NOT NULL CHECK (requester_type IN ('user', 'system')),
    requester_user_id @UUID@ NULL,
    state VARCHAR(16) NOT NULL CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
    provider_name VARCHAR(128) NULL,
    provider_response_id VARCHAR(128) NULL,
    assistant_message_id @UUID@ NULL,
    error_code VARCHAR(64) NULL,
    reserve_tokens BIGINT NULL,
    max_output_tokens_applied INTEGER NULL,
    reserved_credits_micro BIGINT NULL,
    policy_version_applied BIGINT NULL,
    effective_model VARCHAR(1024) NULL,
    minimal_generation_floor_applied INTEGER NULL,
    error_detail TEXT NULL,
    deleted_at @TS@ NULL,
    replaced_by_request_id @UUID@ NULL,
    started_at @TS@ NOT NULL,
    last_progress_at @TS@ NULL,
    web_search_enabled BOOLEAN NOT NULL DEFAULT false,
    web_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (web_search_completed_count >= 0),
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (code_interpreter_completed_count >= 0),
    file_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (file_search_completed_count >= 0),
    completed_at @TS@ NULL,
    updated_at @TS@ NOT NULL,
    UNIQUE (chat_id, request_id)
)";

const ATTACHMENTS: &str = r"
CREATE TABLE IF NOT EXISTS attachments (
    id @UUID@ PRIMARY KEY NOT NULL,
    tenant_id @UUID@ NOT NULL,
    chat_id @UUID@ NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    uploaded_by_user_id @UUID@ NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128) NOT NULL,
    size_bytes BIGINT NOT NULL,
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128) NULL,
    status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    error_code VARCHAR(64) NULL,
    attachment_kind VARCHAR(16) NOT NULL CHECK (attachment_kind IN ('document', 'image')),
    for_file_search BOOLEAN NOT NULL DEFAULT false,
    for_code_interpreter BOOLEAN NOT NULL DEFAULT false,
    doc_summary TEXT NULL,
    img_thumbnail @BYTES@ NULL,
    img_thumbnail_width INTEGER NULL,
    img_thumbnail_height INTEGER NULL,
    summary_model VARCHAR(1024) NULL,
    summary_updated_at @TS@ NULL,
    cleanup_status VARCHAR(16) NULL,
    cleanup_attempts INTEGER NOT NULL DEFAULT 0,
    last_cleanup_error TEXT NULL,
    cleanup_updated_at @TS@ NULL,
    created_at @TS@ NOT NULL,
    updated_at @TS@ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at @TS@ NULL,
    secondary_file_id VARCHAR(128) NULL,
    secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted'
        CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
    secondary_provider_kind VARCHAR(32) NULL
        CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind = 'anthropic'),
    UNIQUE (id, chat_id)
)";

const MESSAGE_ATTACHMENTS: &str = r"
CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id @UUID@ NOT NULL,
    chat_id @UUID@ NOT NULL,
    message_id @UUID@ NOT NULL,
    attachment_id @UUID@ NOT NULL,
    created_at @TS@ NOT NULL,
    PRIMARY KEY (chat_id, message_id, attachment_id),
    FOREIGN KEY (message_id, chat_id) REFERENCES messages (id, chat_id) ON DELETE CASCADE,
    FOREIGN KEY (attachment_id, chat_id) REFERENCES attachments (id, chat_id) ON DELETE CASCADE
)";

const THREAD_SUMMARIES: &str = r"
CREATE TABLE IF NOT EXISTS thread_summaries (
    id @UUID@ PRIMARY KEY NOT NULL,
    tenant_id @UUID@ NOT NULL,
    chat_id @UUID@ NOT NULL UNIQUE REFERENCES chats (id) ON DELETE CASCADE,
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at @TS@ NOT NULL,
    summarized_up_to_message_id @UUID@ NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    created_at @TS@ NOT NULL,
    updated_at @TS@ NOT NULL
)";

const CHAT_VECTOR_STORES: &str = r"
CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id @UUID@ PRIMARY KEY NOT NULL,
    tenant_id @UUID@ NOT NULL,
    chat_id @UUID@ NOT NULL,
    vector_store_id VARCHAR(128) NULL,
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at @TS@ NOT NULL,
    UNIQUE (tenant_id, chat_id)
)";

const QUOTA_USAGE: &str = r"
CREATE TABLE IF NOT EXISTS quota_usage (
    id @UUID@ PRIMARY KEY NOT NULL,
    tenant_id @UUID@ NOT NULL,
    user_id @UUID@ NOT NULL,
    period_type VARCHAR(16) NOT NULL,
    period_start @DATE@ NOT NULL,
    bucket VARCHAR(32) NOT NULL,
    spent_credits_micro BIGINT NOT NULL DEFAULT 0,
    reserved_credits_micro BIGINT NOT NULL DEFAULT 0,
    calls INTEGER NOT NULL DEFAULT 0,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    file_search_calls INTEGER NOT NULL DEFAULT 0,
    web_search_calls INTEGER NOT NULL DEFAULT 0,
    code_interpreter_calls INTEGER NOT NULL DEFAULT 0,
    rag_retrieval_calls INTEGER NOT NULL DEFAULT 0,
    image_inputs INTEGER NOT NULL DEFAULT 0,
    image_upload_bytes BIGINT NOT NULL DEFAULT 0,
    updated_at @TS@ NOT NULL,
    UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
)";

const MESSAGE_REACTIONS: &str = r"
CREATE TABLE IF NOT EXISTS message_reactions (
    id @UUID@ PRIMARY KEY NOT NULL,
    message_id @UUID@ NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id @UUID@ NOT NULL,
    tenant_id @UUID@ NOT NULL,
    reaction VARCHAR(16) NOT NULL CHECK (reaction IN ('like', 'dislike')),
    created_at @TS@ NOT NULL,
    UNIQUE (message_id, user_id)
)";

/// Tables in dependency order.
const TABLES: [&str; 9] = [
    CHATS,
    MESSAGES,
    CHAT_TURNS,
    ATTACHMENTS,
    MESSAGE_ATTACHMENTS,
    THREAD_SUMMARIES,
    CHAT_VECTOR_STORES,
    QUOTA_USAGE,
    MESSAGE_REACTIONS,
];

/// Indexes; partial indexes use `WHERE`, supported by both engines. The unique
/// keys of the tables serve the remaining lookups.
const INDEXES: [&str; 11] = [
    "CREATE INDEX IF NOT EXISTS idx_chats_tenant_user_updated \
     ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL",
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role \
     ON messages (chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_messages_chat_created \
     ON messages (chat_id, created_at) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started \
     ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL",
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_one_running \
     ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_chat_turns_orphan_scan \
     ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat \
     ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_attachments_cleanup_status \
     ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL",
    // A plain index: the scan binds the status values, so a partial predicate
    // would not be usable.
    "CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload \
     ON attachments (status, cleanup_status, deleted_at, updated_at)",
    "CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat \
     ON message_attachments (tenant_id, chat_id)",
    "CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment_chat \
     ON message_attachments (attachment_id, chat_id)",
];
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-create-tables

/// Tables dropped by `down`, children first.
const DROP_ORDER: [&str; 9] = [
    "message_reactions",
    "chat_vector_stores",
    "thread_summaries",
    "quota_usage",
    "message_attachments",
    "attachments",
    "chat_turns",
    "messages",
    "chats",
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-detect-engine
        let dialect = match manager.get_database_backend() {
            DatabaseBackend::Postgres => POSTGRES,
            DatabaseBackend::Sqlite => SQLITE,
            other => {
                return Err(DbErr::Custom(format!(
                    "mini-chat migrations: {other:?} is not supported (PostgreSQL and SQLite only)"
                )));
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-detect-engine

        let conn = manager.get_connection();
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-create-tables
        for table in TABLES {
            conn.execute_unprepared(&dialect.render(table)).await?;
        }
        for index in INDEXES {
            conn.execute_unprepared(index).await?;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-create-tables

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-return-schema
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-run-migrations:p1:inst-return-schema
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        for table in DROP_ORDER {
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table}"))
                .await?;
        }
        Ok(())
    }
}
