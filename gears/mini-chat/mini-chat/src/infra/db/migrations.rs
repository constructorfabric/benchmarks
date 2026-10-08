//! Gear schema (DESIGN §3.7). Raw SQL per backend so CHECKs and partial indexes are kept.
//! SQLite declares UUID columns `TEXT`; `SeaORM` binds the values as 16-byte BLOBs.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(M0001InitialSchema)]
    }
}

pub struct M0001InitialSchema;

impl MigrationName for M0001InitialSchema {
    fn name(&self) -> &'static str {
        "m0001_mini_chat_initial_schema"
    }
}

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS chats (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    user_id {UUID} NOT NULL,
    model VARCHAR(1024) NOT NULL,
    title VARCHAR(255) NULL,
    is_temporary BOOLEAN NOT NULL DEFAULT FALSE,
    created_at {TS} NOT NULL,
    updated_at {TS} NOT NULL,
    deleted_at {TS} NULL
);
CREATE INDEX IF NOT EXISTS idx_chats_owner_updated ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS messages (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    request_id {UUID} NULL,
    role VARCHAR(16) NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL DEFAULT 'text',
    token_estimate INTEGER NOT NULL DEFAULT 0,
    provider_response_id VARCHAR(128) NULL,
    request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
    features_used {JSON} NOT NULL DEFAULT '[]',
    input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens BIGINT NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cache_read_input_tokens BIGINT NOT NULL DEFAULT 0,
    cache_write_input_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    model VARCHAR(1024) NULL,
    is_compressed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at {TS} NOT NULL,
    deleted_at {TS} NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role ON messages (chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_messages_chat_created ON messages (chat_id, created_at) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_id_chat ON messages (id, chat_id);

CREATE TABLE IF NOT EXISTS chat_turns (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
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
    web_search_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    web_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (web_search_completed_count >= 0),
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (code_interpreter_completed_count >= 0),
    file_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (file_search_completed_count >= 0),
    completed_at {TS} NULL,
    updated_at {TS} NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_chat_request ON chat_turns (chat_id, request_id);
CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_one_running ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_chat_turns_orphan_scan ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS attachments (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    uploaded_by_user_id {UUID} NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128) NOT NULL,
    size_bytes BIGINT NOT NULL DEFAULT 0 CHECK (size_bytes >= 0),
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128) NULL,
    status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    error_code VARCHAR(64) NULL,
    attachment_kind VARCHAR(16) NOT NULL CHECK (attachment_kind IN ('document', 'image')),
    for_file_search BOOLEAN NOT NULL DEFAULT FALSE,
    for_code_interpreter BOOLEAN NOT NULL DEFAULT FALSE,
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
);
CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_attachments_cleanup ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload ON attachments (status, cleanup_status, deleted_at, updated_at);
CREATE UNIQUE INDEX IF NOT EXISTS uq_attachments_id_chat ON attachments (id, chat_id);

CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL,
    message_id {UUID} NOT NULL,
    attachment_id {UUID} NOT NULL,
    created_at {TS} NOT NULL,
    PRIMARY KEY (chat_id, message_id, attachment_id),
    FOREIGN KEY (message_id, chat_id) REFERENCES messages (id, chat_id) ON DELETE CASCADE,
    FOREIGN KEY (attachment_id, chat_id) REFERENCES attachments (id, chat_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat ON message_attachments (tenant_id, chat_id);
CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment ON message_attachments (attachment_id, chat_id);

CREATE TABLE IF NOT EXISTS thread_summaries (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at {TS} NOT NULL,
    summarized_up_to_message_id {UUID} NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    created_at {TS} NOT NULL,
    updated_at {TS} NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_thread_summaries_chat ON thread_summaries (chat_id);

CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    chat_id {UUID} NOT NULL,
    vector_store_id VARCHAR(128) NULL,
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at {TS} NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_vector_stores_tenant_chat ON chat_vector_stores (tenant_id, chat_id);

CREATE TABLE IF NOT EXISTS quota_usage (
    id {UUID} PRIMARY KEY NOT NULL,
    tenant_id {UUID} NOT NULL,
    user_id {UUID} NOT NULL,
    period_type VARCHAR(16) NOT NULL CHECK (period_type IN ('daily', 'monthly')),
    period_start {DATE} NOT NULL,
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
    updated_at {TS} NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_quota_usage_key ON quota_usage (tenant_id, user_id, period_type, period_start, bucket);

CREATE TABLE IF NOT EXISTS message_reactions (
    id {UUID} PRIMARY KEY NOT NULL,
    message_id {UUID} NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id {UUID} NOT NULL,
    tenant_id {UUID} NOT NULL,
    reaction VARCHAR(16) NOT NULL CHECK (reaction IN ('like', 'dislike')),
    created_at {TS} NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_message_reactions_message_user ON message_reactions (message_id, user_id);
";

/// Renders the schema for one backend.
#[must_use]
pub fn render_schema(backend: sea_orm::DatabaseBackend) -> Option<String> {
    let (uuid, ts, bytes, json, date) = match backend {
        sea_orm::DatabaseBackend::Postgres => ("UUID", "TIMESTAMPTZ", "BYTEA", "JSONB", "DATE"),
        sea_orm::DatabaseBackend::Sqlite => ("TEXT", "TEXT", "BLOB", "TEXT", "TEXT"),
        _ => return None,
    };
    Some(
        SCHEMA
            .replace("{UUID}", uuid)
            .replace("{TS}", ts)
            .replace("{BYTES}", bytes)
            .replace("{JSON}", json)
            .replace("{DATE}", date),
    )
}

#[async_trait::async_trait]
impl MigrationTrait for M0001InitialSchema {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        let sql = render_schema(manager.get_database_backend()).ok_or_else(|| {
            DbErr::Custom("mini-chat migrations support PostgreSQL and SQLite only".to_owned())
        })?;
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            conn.execute_unprepared(stmt).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        for t in [
            "message_reactions",
            "quota_usage",
            "chat_vector_stores",
            "thread_summaries",
            "message_attachments",
            "attachments",
            "chat_turns",
            "messages",
            "chats",
        ] {
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {t}"))
                .await?;
        }
        Ok(())
    }
}
