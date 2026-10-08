//! Initial mini-chat schema (D§3.7): `chats`, `messages`, `chat_turns`,
//! `attachments`, `message_attachments`, `thread_summaries`,
//! `chat_vector_stores`, `quota_usage`, `message_reactions`.
//!
//! Raw SQL per backend (CHECKs, partial indexes and composite FKs verbatim).
//! One DDL template is rendered for the detected backend: PostgreSQL uses
//! `UUID` / `TIMESTAMPTZ` / `JSONB` / `BYTEA` / `DATE` / `BOOLEAN`; `SQLite`
//! declares UUID columns `TEXT` (values are written as 16-byte BLOBs by the
//! ORM), timestamps / JSON / dates as `TEXT`, bytes as `BLOB`. MySQL is not
//! supported.
//!
//! Not enforced here by design (ADR-0010): the cross-column CHECKs of
//! `chat_turns` (`completed_at` / `last_progress_at` vs `state`) and the
//! `attachments.cleanup_status` value set — the repositories keep them.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend};

const MYSQL_NOT_SUPPORTED: &str =
    "mini-chat migrations: MySQL is not supported (PostgreSQL/SQLite only)";

/// Table DDL; placeholders are substituted per backend by [`render`].
const TABLES: &[&str] = &[
    r"CREATE TABLE IF NOT EXISTS chats (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    user_id {uuid} NOT NULL,
    model VARCHAR(1024) NOT NULL,
    title VARCHAR(255) NULL,
    is_temporary BOOLEAN NOT NULL DEFAULT {false},
    created_at {ts} NOT NULL,
    updated_at {ts} NOT NULL,
    deleted_at {ts} NULL
)",
    r"CREATE TABLE IF NOT EXISTS messages (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    request_id {uuid} NULL,
    role VARCHAR(16) NOT NULL,
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL DEFAULT 'text',
    token_estimate INTEGER NOT NULL DEFAULT 0,
    provider_response_id VARCHAR(128) NULL,
    request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
    features_used {json} NOT NULL DEFAULT {empty_json_array},
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cache_read_input_tokens BIGINT NOT NULL DEFAULT 0,
    cache_write_input_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    model VARCHAR(1024) NULL,
    is_compressed BOOLEAN NOT NULL DEFAULT {false},
    created_at {ts} NOT NULL,
    deleted_at {ts} NULL,
    CONSTRAINT uq_messages_id_chat UNIQUE (id, chat_id)
)",
    r"CREATE TABLE IF NOT EXISTS chat_turns (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    request_id {uuid} NOT NULL,
    requester_type VARCHAR(16) NOT NULL CHECK (requester_type IN ('user', 'system')),
    requester_user_id {uuid} NULL,
    state VARCHAR(16) NOT NULL CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
    provider_name VARCHAR(128) NULL,
    provider_response_id VARCHAR(128) NULL,
    assistant_message_id {uuid} NULL,
    error_code VARCHAR(64) NULL,
    reserve_tokens BIGINT NULL,
    max_output_tokens_applied INTEGER NULL,
    reserved_credits_micro BIGINT NULL,
    policy_version_applied BIGINT NULL,
    effective_model VARCHAR(1024) NULL,
    minimal_generation_floor_applied INTEGER NULL,
    error_detail TEXT NULL,
    deleted_at {ts} NULL,
    replaced_by_request_id {uuid} NULL,
    started_at {ts} NOT NULL,
    last_progress_at {ts} NULL,
    web_search_enabled BOOLEAN NOT NULL DEFAULT {false},
    web_search_completed_count INTEGER NOT NULL DEFAULT 0,
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0,
    file_search_completed_count INTEGER NOT NULL DEFAULT 0,
    completed_at {ts} NULL,
    updated_at {ts} NOT NULL,
    CONSTRAINT uq_chat_turns_chat_request UNIQUE (chat_id, request_id)
)",
    r"CREATE TABLE IF NOT EXISTS attachments (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    uploaded_by_user_id {uuid} NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128) NOT NULL,
    size_bytes BIGINT NOT NULL,
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128) NULL,
    status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    error_code VARCHAR(64) NULL,
    attachment_kind VARCHAR(16) NOT NULL CHECK (attachment_kind IN ('document', 'image')),
    for_file_search BOOLEAN NOT NULL DEFAULT {false},
    for_code_interpreter BOOLEAN NOT NULL DEFAULT {false},
    doc_summary TEXT NULL,
    img_thumbnail {bytes} NULL,
    img_thumbnail_width INTEGER NULL,
    img_thumbnail_height INTEGER NULL,
    summary_model VARCHAR(1024) NULL,
    summary_updated_at {ts} NULL,
    cleanup_status VARCHAR(16) NULL,
    cleanup_attempts INTEGER NOT NULL DEFAULT 0,
    last_cleanup_error TEXT NULL,
    cleanup_updated_at {ts} NULL,
    created_at {ts} NOT NULL,
    updated_at {ts} NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at {ts} NULL,
    secondary_file_id VARCHAR(128) NULL,
    secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted'
        CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
    secondary_provider_kind VARCHAR(32) NULL
        CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind = 'anthropic'),
    CONSTRAINT uq_attachments_id_chat UNIQUE (id, chat_id)
)",
    r"CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    message_id {uuid} NOT NULL,
    attachment_id {uuid} NOT NULL,
    created_at {ts} NOT NULL,
    PRIMARY KEY (chat_id, message_id, attachment_id),
    CONSTRAINT fk_message_attachments_message FOREIGN KEY (message_id, chat_id)
        REFERENCES messages(id, chat_id) ON DELETE CASCADE,
    CONSTRAINT fk_message_attachments_attachment FOREIGN KEY (attachment_id, chat_id)
        REFERENCES attachments(id, chat_id) ON DELETE CASCADE
)",
    r"CREATE TABLE IF NOT EXISTS thread_summaries (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at {ts} NOT NULL,
    summarized_up_to_message_id {uuid} NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    created_at {ts} NOT NULL,
    updated_at {ts} NOT NULL,
    CONSTRAINT uq_thread_summaries_chat UNIQUE (chat_id)
)",
    r"CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    vector_store_id VARCHAR(128) NULL,
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at {ts} NOT NULL,
    CONSTRAINT uq_chat_vector_stores_tenant_chat UNIQUE (tenant_id, chat_id)
)",
    r"CREATE TABLE IF NOT EXISTS quota_usage (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    user_id {uuid} NOT NULL,
    period_type VARCHAR(16) NOT NULL,
    period_start {date} NOT NULL,
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
    updated_at {ts} NOT NULL,
    CONSTRAINT uq_quota_usage_bucket UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
)",
    r"CREATE TABLE IF NOT EXISTS message_reactions (
    id {uuid} PRIMARY KEY NOT NULL,
    message_id {uuid} NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id {uuid} NOT NULL,
    tenant_id {uuid} NOT NULL,
    reaction VARCHAR(16) NOT NULL CHECK (reaction IN ('like', 'dislike')),
    created_at {ts} NOT NULL,
    CONSTRAINT uq_message_reactions_message_user UNIQUE (message_id, user_id)
)",
];

/// Index DDL (identical on both backends; partial indexes included).
const INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_chats_tenant_user_updated \
     ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL",
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role \
     ON messages (chat_id, request_id, role) \
     WHERE request_id IS NOT NULL AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_messages_chat_created \
     ON messages (chat_id, created_at) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started \
     ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL",
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_one_running \
     ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_chat_turns_running_progress \
     ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat \
     ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_attachments_cleanup_status \
     ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload \
     ON attachments (status, cleanup_status, deleted_at, updated_at)",
    "CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat \
     ON message_attachments (tenant_id, chat_id)",
    "CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment_chat \
     ON message_attachments (attachment_id, chat_id)",
];

/// Drop order (children first).
const DROP_ORDER: &[&str] = &[
    "message_reactions",
    "message_attachments",
    "thread_summaries",
    "chat_vector_stores",
    "quota_usage",
    "attachments",
    "chat_turns",
    "messages",
    "chats",
];

/// Render a DDL template for `backend`.
///
/// # Errors
///
/// `DbErr::Custom` for MySQL (unsupported).
pub fn render(template: &str, backend: DatabaseBackend) -> Result<String, DbErr> {
    let subs: [(&str, &str); 7] = match backend {
        DatabaseBackend::Postgres => [
            ("{uuid}", "UUID"),
            ("{ts}", "TIMESTAMPTZ"),
            ("{json}", "JSONB"),
            ("{empty_json_array}", "'[]'::jsonb"),
            ("{bytes}", "BYTEA"),
            ("{date}", "DATE"),
            ("{false}", "FALSE"),
        ],
        DatabaseBackend::Sqlite => [
            ("{uuid}", "TEXT"),
            ("{ts}", "TEXT"),
            ("{json}", "TEXT"),
            ("{empty_json_array}", "'[]'"),
            ("{bytes}", "BLOB"),
            ("{date}", "TEXT"),
            ("{false}", "0"),
        ],
        _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
    };
    let mut sql = template.to_owned();
    for (from, to) in subs {
        sql = sql.replace(from, to);
    }
    Ok(sql)
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        for table in TABLES {
            conn.execute_unprepared(&render(table, backend)?).await?;
        }
        for index in INDEXES {
            conn.execute_unprepared(&render(index, backend)?).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        if matches!(backend, DatabaseBackend::MySql) {
            return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned()));
        }
        let conn = manager.get_connection();
        for table in DROP_ORDER {
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table}"))
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "m20261004_000001_initial_tests.rs"]
mod m20261004_000001_initial_tests;
