//! Initial mini-chat schema (DESIGN §3.7) for PostgreSQL and SQLite.
//!
//! The DDL is written once with type placeholders that are substituted per
//! backend: `{UUID}` (UUID / BLOB), `{TS}` (TIMESTAMPTZ / TEXT), `{DATE}`
//! (DATE / TEXT), `{BOOL}` (BOOLEAN), `{BIGINT}`, `{JSON}` (JSONB / TEXT),
//! `{BYTES}` (BYTEA / BLOB).

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

#[derive(DeriveMigrationName)]
pub struct Migration;

const TABLES: &[&str] = &[
    // chats
    r"CREATE TABLE IF NOT EXISTS chats (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    user_id {UUID} NOT NULL,
    model VARCHAR(1024) NOT NULL,
    title VARCHAR(255) NULL,
    is_temporary {BOOL} NOT NULL DEFAULT FALSE,
    created_at {TS} NOT NULL,
    updated_at {TS} NOT NULL,
    deleted_at {TS} NULL
)",
    r"CREATE INDEX IF NOT EXISTS idx_chats_owner_updated ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL",
    // messages
    r"CREATE TABLE IF NOT EXISTS messages (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id {UUID} NULL,
    role VARCHAR(16) NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL DEFAULT 'text',
    token_estimate INTEGER NOT NULL DEFAULT 0,
    provider_response_id VARCHAR(128) NULL,
    request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
    features_used {JSON} NOT NULL DEFAULT '[]',
    input_tokens {BIGINT} NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens {BIGINT} NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cache_read_input_tokens {BIGINT} NOT NULL DEFAULT 0,
    cache_write_input_tokens {BIGINT} NOT NULL DEFAULT 0,
    reasoning_tokens {BIGINT} NOT NULL DEFAULT 0,
    model VARCHAR(1024) NULL,
    is_compressed {BOOL} NOT NULL DEFAULT FALSE,
    created_at {TS} NOT NULL,
    deleted_at {TS} NULL
)",
    r"CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role ON messages (chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL",
    r"CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_id_chat ON messages (id, chat_id)",
    r"CREATE INDEX IF NOT EXISTS idx_messages_chat_created ON messages (chat_id, created_at) WHERE deleted_at IS NULL",
    // chat_turns
    r"CREATE TABLE IF NOT EXISTS chat_turns (
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
    reserve_tokens {BIGINT} NULL,
    max_output_tokens_applied INTEGER NULL,
    reserved_credits_micro {BIGINT} NULL,
    policy_version_applied {BIGINT} NULL,
    effective_model VARCHAR(1024) NULL,
    minimal_generation_floor_applied INTEGER NULL,
    error_detail TEXT NULL,
    deleted_at {TS} NULL,
    replaced_by_request_id {UUID} NULL,
    started_at {TS} NOT NULL,
    last_progress_at {TS} NULL,
    web_search_enabled {BOOL} NOT NULL DEFAULT FALSE,
    web_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (web_search_completed_count >= 0),
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (code_interpreter_completed_count >= 0),
    file_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (file_search_completed_count >= 0),
    completed_at {TS} NULL,
    updated_at {TS} NOT NULL
)",
    r"CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_chat_request ON chat_turns (chat_id, request_id)",
    r"CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_one_running ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL",
    r"CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL",
    r"CREATE INDEX IF NOT EXISTS idx_chat_turns_running_progress ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL",
    // attachments
    r"CREATE TABLE IF NOT EXISTS attachments (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    uploaded_by_user_id {UUID} NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128) NOT NULL,
    size_bytes {BIGINT} NOT NULL DEFAULT 0 CHECK (size_bytes >= 0),
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128) NULL,
    status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    error_code VARCHAR(64) NULL,
    attachment_kind VARCHAR(16) NOT NULL CHECK (attachment_kind IN ('document', 'image')),
    for_file_search {BOOL} NOT NULL DEFAULT FALSE,
    for_code_interpreter {BOOL} NOT NULL DEFAULT FALSE,
    doc_summary TEXT NULL,
    img_thumbnail {BYTES} NULL,
    img_thumbnail_width INTEGER NULL,
    img_thumbnail_height INTEGER NULL,
    summary_model VARCHAR(1024) NULL,
    summary_updated_at {TS} NULL,
    cleanup_status VARCHAR(16) NULL,
    cleanup_attempts INTEGER NOT NULL DEFAULT 0 CHECK (cleanup_attempts >= 0),
    last_cleanup_error TEXT NULL,
    cleanup_updated_at {TS} NULL,
    created_at {TS} NOT NULL,
    updated_at {TS} NOT NULL,
    deleted_at {TS} NULL,
    secondary_file_id VARCHAR(128) NULL,
    secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted' CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
    secondary_provider_kind VARCHAR(32) NULL CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic'))
)",
    r"CREATE UNIQUE INDEX IF NOT EXISTS uq_attachments_id_chat ON attachments (id, chat_id)",
    r"CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL",
    r"CREATE INDEX IF NOT EXISTS idx_attachments_cleanup ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL",
    r"CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload ON attachments (status, cleanup_status, deleted_at, updated_at)",
    // message_attachments
    r"CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL,
    message_id {UUID} NOT NULL,
    attachment_id {UUID} NOT NULL,
    created_at {TS} NOT NULL,
    PRIMARY KEY (chat_id, message_id, attachment_id),
    FOREIGN KEY (message_id, chat_id) REFERENCES messages (id, chat_id) ON DELETE CASCADE,
    FOREIGN KEY (attachment_id, chat_id) REFERENCES attachments (id, chat_id) ON DELETE CASCADE
)",
    r"CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat ON message_attachments (tenant_id, chat_id)",
    r"CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment ON message_attachments (attachment_id, chat_id)",
    // thread_summaries
    r"CREATE TABLE IF NOT EXISTS thread_summaries (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL UNIQUE REFERENCES chats (id) ON DELETE CASCADE,
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at {TS} NOT NULL,
    summarized_up_to_message_id {UUID} NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    created_at {TS} NOT NULL,
    updated_at {TS} NOT NULL
)",
    // chat_vector_stores
    r"CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL,
    vector_store_id VARCHAR(128) NULL,
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at {TS} NOT NULL
)",
    r"CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_vector_stores_tenant_chat ON chat_vector_stores (tenant_id, chat_id)",
    // quota_usage
    r"CREATE TABLE IF NOT EXISTS quota_usage (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    user_id {UUID} NOT NULL,
    period_type VARCHAR(16) NOT NULL,
    period_start {DATE} NOT NULL,
    bucket VARCHAR(32) NOT NULL,
    spent_credits_micro {BIGINT} NOT NULL DEFAULT 0,
    reserved_credits_micro {BIGINT} NOT NULL DEFAULT 0,
    calls INTEGER NOT NULL DEFAULT 0,
    input_tokens {BIGINT} NOT NULL DEFAULT 0,
    output_tokens {BIGINT} NOT NULL DEFAULT 0,
    file_search_calls INTEGER NOT NULL DEFAULT 0,
    web_search_calls INTEGER NOT NULL DEFAULT 0,
    code_interpreter_calls INTEGER NOT NULL DEFAULT 0,
    rag_retrieval_calls INTEGER NOT NULL DEFAULT 0,
    image_inputs INTEGER NOT NULL DEFAULT 0,
    image_upload_bytes {BIGINT} NOT NULL DEFAULT 0,
    updated_at {TS} NOT NULL
)",
    r"CREATE UNIQUE INDEX IF NOT EXISTS uq_quota_usage_key ON quota_usage (tenant_id, user_id, period_type, period_start, bucket)",
    // message_reactions
    r"CREATE TABLE IF NOT EXISTS message_reactions (
    id {UUID} PRIMARY KEY NOT NULL,
    message_id {UUID} NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id {UUID} NOT NULL,
    tenant_id {UUID} NOT NULL,
    reaction VARCHAR(16) NOT NULL CHECK (reaction IN ('like', 'dislike')),
    created_at {TS} NOT NULL
)",
    r"CREATE UNIQUE INDEX IF NOT EXISTS uq_message_reactions_message_user ON message_reactions (message_id, user_id)",
];

const DROP: &[&str] = &[
    "DROP TABLE IF EXISTS message_reactions",
    "DROP TABLE IF EXISTS quota_usage",
    "DROP TABLE IF EXISTS chat_vector_stores",
    "DROP TABLE IF EXISTS thread_summaries",
    "DROP TABLE IF EXISTS message_attachments",
    "DROP TABLE IF EXISTS attachments",
    "DROP TABLE IF EXISTS chat_turns",
    "DROP TABLE IF EXISTS messages",
    "DROP TABLE IF EXISTS chats",
];

fn render(sql: &str, backend: DatabaseBackend) -> Result<String, DbErr> {
    let (uuid, ts, date, boolean, bigint, json, bytes) = match backend {
        DatabaseBackend::Postgres => ("UUID", "TIMESTAMPTZ", "DATE", "BOOLEAN", "BIGINT", "JSONB", "BYTEA"),
        DatabaseBackend::Sqlite => ("BLOB", "TEXT", "TEXT", "BOOLEAN", "BIGINT", "TEXT", "BLOB"),
        other => {
            return Err(DbErr::Custom(format!(
                "mini-chat migrations support PostgreSQL and SQLite only, got {other:?}"
            )));
        }
    };
    Ok(sql
        .replace("{UUID}", uuid)
        .replace("{TS}", ts)
        .replace("{DATE}", date)
        .replace("{BOOL}", boolean)
        .replace("{BIGINT}", bigint)
        .replace("{JSON}", json)
        .replace("{BYTES}", bytes))
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        for sql in TABLES {
            let sql = render(sql, backend)?;
            conn.execute_raw(Statement::from_string(backend, sql)).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        for sql in DROP {
            conn.execute_raw(Statement::from_string(backend, (*sql).to_owned())).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(backend: DatabaseBackend) -> String {
        TABLES.iter().map(|s| render(s, backend).expect("render")).collect::<Vec<_>>().join(";\n")
    }

    #[test]
    fn postgres_ddl_uses_native_types() {
        let sql = all(DatabaseBackend::Postgres);
        assert!(!sql.contains('{'), "no placeholder left");
        for ty in ["UUID", "TIMESTAMPTZ", "JSONB", "BYTEA", "BOOLEAN", "BIGINT", "DATE"] {
            assert!(sql.contains(ty), "{ty} missing");
        }
        assert!(!sql.contains("BLOB"));
        assert!(sql.contains(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_one_running ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL"
        ));
        assert!(sql.contains("uq_chat_turns_chat_request ON chat_turns (chat_id, request_id)"));
        assert!(sql.contains("uq_quota_usage_key ON quota_usage (tenant_id, user_id, period_type, period_start, bucket)"));
        for table in [
            "chats", "messages", "chat_turns", "attachments", "message_attachments", "thread_summaries",
            "chat_vector_stores", "quota_usage", "message_reactions",
        ] {
            assert!(sql.contains(&format!("CREATE TABLE IF NOT EXISTS {table} ")), "{table} missing");
        }
    }

    #[test]
    fn sqlite_ddl_has_no_placeholders_and_other_backends_fail() {
        let sql = all(DatabaseBackend::Sqlite);
        assert!(!sql.contains('{'));
        assert!(!sql.contains("TIMESTAMPTZ") && !sql.contains("JSONB"));
        assert!(render("x", DatabaseBackend::MySql).is_err());
    }
}
