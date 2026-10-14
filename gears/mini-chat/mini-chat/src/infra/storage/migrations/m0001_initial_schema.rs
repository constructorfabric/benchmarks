//! Initial schema: `chats`, `messages`, `chat_turns`, `attachments`, `message_attachments`,
//! `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions`.
//!
//! Per-backend raw SQL (the only place raw SQL is allowed). One statement list serves both
//! engines; the dialect differences are the column types (a PostgreSQL variant and a SQLite
//! variant, see [`Dialect`]). UUID columns are `TEXT` in `SQLite` and hold 16-byte `BLOB`
//! values (the UUID bytes, not the text form). `MySQL` is not supported.
//!
//! The cross-column CHECKs on `chat_turns` (`completed_at` and `last_progress_at` against
//! `state`) and the value CHECK on `attachments.cleanup_status` are deliberately not created:
//! the repositories keep those invariants (ADR-0010).

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend};

const MYSQL_NOT_SUPPORTED: &str = "mini-chat migrations: MySQL is not supported \
    (this migration set targets PostgreSQL/SQLite)";

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-dialects
/// The two supported SQL dialects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Postgres,
    Sqlite,
}

impl Dialect {
    fn of(backend: DatabaseBackend) -> Result<Self, DbErr> {
        match backend {
            DatabaseBackend::Postgres => Ok(Self::Postgres),
            DatabaseBackend::Sqlite => Ok(Self::Sqlite),
            _ => Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        }
    }

    /// Substitute the dialect-specific column types into a statement template.
    fn render(self, template: &str) -> String {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-uuid-blob
        // UUID columns are `TEXT` in SQLite; the values written into them are 16-byte blobs
        let (uuid, timestamp, bytes, json_array) = match self {
            Self::Postgres => (
                "UUID",
                "TIMESTAMPTZ",
                "BYTEA",
                "JSONB NOT NULL DEFAULT '[]'::jsonb",
            ),
            Self::Sqlite => ("TEXT", "TEXT", "BLOB", "TEXT NOT NULL DEFAULT '[]'"),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-uuid-blob
        template
            .replace("{UUID}", uuid)
            .replace("{TS}", timestamp)
            .replace("{BYTES}", bytes)
            .replace("{JSON_ARRAY}", json_array)
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-dialects

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-no-deferred-tables
/// The nine P1 tables in creation order; `down` drops them in reverse. No `mcp_servers`,
/// `mcp_server_tools`, `role_mcp_servers`, `tenant_closure` or policy snapshot table exists.
const TABLES: [&str; 9] = [
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
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-no-deferred-tables

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-chats
const CHATS: &str = r"
CREATE TABLE IF NOT EXISTS chats (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    user_id {UUID} NOT NULL,
    model VARCHAR(1024) NOT NULL,
    title VARCHAR(255) NULL,
    is_temporary BOOLEAN NOT NULL DEFAULT false,
    created_at {TS} NOT NULL,
    updated_at {TS} NOT NULL,
    deleted_at {TS} NULL
)
";

const CHATS_LISTING_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_chats_tenant_user_updated \
     ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL";
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-chats

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-messages
const MESSAGES: &str = r"
CREATE TABLE IF NOT EXISTS messages (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id {UUID} NULL,
    role VARCHAR(16) NOT NULL,
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    provider_response_id VARCHAR(128) NULL,
    request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
    features_used {JSON_ARRAY},
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cache_read_input_tokens BIGINT NOT NULL DEFAULT 0,
    cache_write_input_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    model VARCHAR(1024) NULL,
    is_compressed BOOLEAN NOT NULL DEFAULT false,
    created_at {TS} NOT NULL,
    deleted_at {TS} NULL,
    UNIQUE (id, chat_id)
)
";

const MESSAGES_REQUEST_ROLE_INDEX: &str = "CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role \
     ON messages (chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL";

const MESSAGES_SCAN_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_messages_chat_created \
     ON messages (chat_id, created_at) WHERE deleted_at IS NULL";
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-messages

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-chat-turns
const CHAT_TURNS: &str = r"
CREATE TABLE IF NOT EXISTS chat_turns (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id {UUID} NOT NULL,
    requester_type VARCHAR(16) NOT NULL CHECK (requester_type IN ('user', 'system')),
    requester_user_id {UUID} NULL,
    state VARCHAR(16) NOT NULL CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
    provider_name VARCHAR(128) NULL,
    provider_response_id VARCHAR(128) NULL,
    assistant_message_id {UUID} NULL,
    error_code VARCHAR(64) NULL,
    reserve_tokens BIGINT NULL,
    max_output_tokens_applied INTEGER NULL,
    reserved_credits_micro BIGINT NULL,
    policy_version_applied BIGINT NULL,
    effective_model VARCHAR(1024) NULL,
    minimal_generation_floor_applied INTEGER NULL,
    error_detail TEXT NULL,
    deleted_at {TS} NULL,
    replaced_by_request_id {UUID} NULL,
    started_at {TS} NOT NULL,
    last_progress_at {TS} NULL,
    web_search_enabled BOOLEAN NOT NULL DEFAULT false,
    web_search_completed_count INTEGER NOT NULL DEFAULT 0,
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0,
    file_search_completed_count INTEGER NOT NULL DEFAULT 0,
    completed_at {TS} NULL,
    updated_at {TS} NOT NULL,
    UNIQUE (chat_id, request_id)
)
";

const CHAT_TURNS_LATEST_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started \
     ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL";

const CHAT_TURNS_RUNNING_INDEX: &str = "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_running \
     ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL";

const CHAT_TURNS_ORPHAN_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_chat_turns_orphan_scan \
     ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL";
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-chat-turns

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-attachments
const ATTACHMENTS: &str = r"
CREATE TABLE IF NOT EXISTS attachments (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    uploaded_by_user_id {UUID} NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128) NULL,
    size_bytes BIGINT NULL,
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128) NULL,
    status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    error_code VARCHAR(64) NULL,
    attachment_kind VARCHAR(16) NOT NULL CHECK (attachment_kind IN ('document', 'image')),
    for_file_search BOOLEAN NOT NULL DEFAULT false,
    for_code_interpreter BOOLEAN NOT NULL DEFAULT false,
    doc_summary TEXT NULL,
    img_thumbnail {BYTES} NULL,
    img_thumbnail_width INTEGER NULL,
    img_thumbnail_height INTEGER NULL,
    summary_model VARCHAR(1024) NULL,
    summary_updated_at {TS} NULL,
    cleanup_status VARCHAR(16) NULL,
    cleanup_attempts INTEGER NOT NULL DEFAULT 0,
    last_cleanup_error TEXT NULL,
    cleanup_updated_at {TS} NULL,
    created_at {TS} NOT NULL,
    updated_at {TS} NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at {TS} NULL,
    secondary_file_id VARCHAR(128) NULL,
    secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted'
        CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
    secondary_provider_kind VARCHAR(32) NULL
        CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic')),
    UNIQUE (id, chat_id)
)
";

const ATTACHMENTS_TENANT_CHAT_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat \
     ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL";

const ATTACHMENTS_CLEANUP_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_attachments_cleanup \
     ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL";

// a plain index: the upload reaper binds the status values as parameters, and SQLite uses a
// partial index only when the query repeats its WHERE literally
const ATTACHMENTS_STALE_UPLOAD_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload \
     ON attachments (status, cleanup_status, deleted_at, updated_at)";
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-attachments

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-message-attachments
const MESSAGE_ATTACHMENTS: &str = r"
CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL,
    message_id {UUID} NOT NULL,
    attachment_id {UUID} NOT NULL,
    created_at {TS} NOT NULL,
    PRIMARY KEY (chat_id, message_id, attachment_id),
    FOREIGN KEY (message_id, chat_id) REFERENCES messages (id, chat_id) ON DELETE CASCADE,
    FOREIGN KEY (attachment_id, chat_id) REFERENCES attachments (id, chat_id) ON DELETE CASCADE
)
";

const MESSAGE_ATTACHMENTS_TENANT_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat \
     ON message_attachments (tenant_id, chat_id)";

const MESSAGE_ATTACHMENTS_REVERSE_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment_chat \
     ON message_attachments (attachment_id, chat_id)";
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-message-attachments

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-thread-summaries
const THREAD_SUMMARIES: &str = r"
CREATE TABLE IF NOT EXISTS thread_summaries (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL UNIQUE REFERENCES chats (id) ON DELETE CASCADE,
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at {TS} NOT NULL,
    summarized_up_to_message_id {UUID} NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    created_at {TS} NOT NULL,
    updated_at {TS} NOT NULL
)
";
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-thread-summaries

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-chat-vector-stores
const CHAT_VECTOR_STORES: &str = r"
CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL,
    vector_store_id VARCHAR(128) NULL,
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at {TS} NOT NULL,
    UNIQUE (tenant_id, chat_id)
)
";
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-chat-vector-stores

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-quota-usage
const QUOTA_USAGE: &str = r"
CREATE TABLE IF NOT EXISTS quota_usage (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    user_id {UUID} NOT NULL,
    period_type VARCHAR(16) NOT NULL,
    period_start DATE NOT NULL,
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
    updated_at {TS} NOT NULL,
    UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
)
";

const QUOTA_USAGE_LOOKUP_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_quota_usage_lookup \
     ON quota_usage (tenant_id, user_id, period_type, period_start, bucket)";
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-quota-usage

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-message-reactions
const MESSAGE_REACTIONS: &str = r"
CREATE TABLE IF NOT EXISTS message_reactions (
    id {UUID} PRIMARY KEY NOT NULL,
    message_id {UUID} NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id {UUID} NOT NULL,
    tenant_id {UUID} NOT NULL,
    reaction VARCHAR(16) NOT NULL CHECK (reaction IN ('like', 'dislike')),
    created_at {TS} NOT NULL,
    UNIQUE (message_id, user_id)
)
";
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-message-reactions

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-unenforced-checks
/// Every statement of the migration in execution order: the tables, then their indexes. The
/// cross-column CHECKs of `chat_turns` and the `attachments.cleanup_status` value CHECK are
/// not part of any statement (ADR-0010).
const STATEMENTS: [&str; 21] = [
    CHATS,
    MESSAGES,
    CHAT_TURNS,
    ATTACHMENTS,
    MESSAGE_ATTACHMENTS,
    THREAD_SUMMARIES,
    CHAT_VECTOR_STORES,
    QUOTA_USAGE,
    MESSAGE_REACTIONS,
    CHATS_LISTING_INDEX,
    MESSAGES_REQUEST_ROLE_INDEX,
    MESSAGES_SCAN_INDEX,
    CHAT_TURNS_LATEST_INDEX,
    CHAT_TURNS_RUNNING_INDEX,
    CHAT_TURNS_ORPHAN_INDEX,
    ATTACHMENTS_TENANT_CHAT_INDEX,
    ATTACHMENTS_CLEANUP_INDEX,
    ATTACHMENTS_STALE_UPLOAD_INDEX,
    MESSAGE_ATTACHMENTS_TENANT_INDEX,
    MESSAGE_ATTACHMENTS_REVERSE_INDEX,
    QUOTA_USAGE_LOOKUP_INDEX,
];
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-unenforced-checks

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let dialect = Dialect::of(manager.get_database_backend())?;
        let conn = manager.get_connection();
        for statement in STATEMENTS {
            conn.execute_unprepared(&dialect.render(statement)).await?;
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-return
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-schema-migration:p1:inst-schema-return
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        Dialect::of(manager.get_database_backend())?;
        let conn = manager.get_connection();
        // children first: the foreign keys reference the parents
        for table in TABLES.iter().rev() {
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table}"))
                .await?;
        }
        Ok(())
    }
}
