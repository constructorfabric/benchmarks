//! `SeaORM` migrations for the mini-chat gear (`PostgreSQL` + SQLite).
//!
//! Table layout follows DESIGN.md section 3.7 exactly. SQLite stores UUIDs
//! as 16-byte BLOBs (the sqlx/`SeaORM` UUID binding writes raw bytes) and
//! timestamps as TEXT.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

const MYSQL_NOT_SUPPORTED: &str = "mini-chat: MySQL is not supported";

const PG_STATEMENTS: &[&str] = &[
    r"
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
);",
    r"
CREATE TABLE IF NOT EXISTS messages (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    request_id UUID,
    role VARCHAR(16) NOT NULL,
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    provider_response_id VARCHAR(128),
    request_kind VARCHAR(16),
    features_used JSONB NOT NULL DEFAULT '[]',
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cache_read_input_tokens BIGINT NOT NULL DEFAULT 0,
    cache_write_input_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    model VARCHAR(1024),
    is_compressed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL,
    deleted_at TIMESTAMPTZ,
    CONSTRAINT uq_messages_id_chat UNIQUE (id, chat_id)
);",
    r"
CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role
    ON messages (chat_id, request_id, role)
    WHERE request_id IS NOT NULL AND deleted_at IS NULL;",
    r"
CREATE INDEX IF NOT EXISTS idx_messages_chat_created
    ON messages (chat_id, created_at)
    WHERE deleted_at IS NULL;",
    r"
CREATE TABLE IF NOT EXISTS chat_turns (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    request_id UUID NOT NULL,
    requester_type VARCHAR(16) NOT NULL,
    requester_user_id UUID,
    state VARCHAR(16) NOT NULL,
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
    web_search_completed_count INTEGER NOT NULL DEFAULT 0,
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0,
    file_search_completed_count INTEGER NOT NULL DEFAULT 0,
    completed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT uq_chat_turns_chat_request UNIQUE (chat_id, request_id),
    CONSTRAINT chk_chat_turns_requester_type CHECK (requester_type IN ('user', 'system')),
    CONSTRAINT chk_chat_turns_state CHECK (state IN ('running', 'completed', 'failed', 'cancelled'))
);",
    r"
CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started
    ON chat_turns (chat_id, started_at DESC)
    WHERE deleted_at IS NULL;",
    r"
CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_one_running
    ON chat_turns (chat_id)
    WHERE state = 'running' AND deleted_at IS NULL;",
    r"
CREATE INDEX IF NOT EXISTS idx_chat_turns_progress
    ON chat_turns (last_progress_at)
    WHERE state = 'running' AND deleted_at IS NULL;",
    r"
CREATE TABLE IF NOT EXISTS attachments (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    uploaded_by_user_id UUID NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128) NOT NULL,
    size_bytes BIGINT NOT NULL,
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128),
    status VARCHAR(16) NOT NULL,
    error_code VARCHAR(64),
    attachment_kind VARCHAR(16) NOT NULL,
    for_file_search BOOLEAN NOT NULL DEFAULT FALSE,
    for_code_interpreter BOOLEAN NOT NULL DEFAULT FALSE,
    doc_summary TEXT,
    img_thumbnail BYTEA,
    img_thumbnail_width INTEGER,
    img_thumbnail_height INTEGER,
    summary_model VARCHAR(1024),
    summary_updated_at TIMESTAMPTZ,
    cleanup_status VARCHAR(16),
    cleanup_attempts INTEGER NOT NULL DEFAULT 0,
    last_cleanup_error TEXT,
    cleanup_updated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMPTZ,
    secondary_file_id VARCHAR(128),
    secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted',
    secondary_provider_kind VARCHAR(32),
    CONSTRAINT uq_attachments_id_chat UNIQUE (id, chat_id),
    CONSTRAINT chk_attachments_kind CHECK (attachment_kind IN ('document', 'image')),
    CONSTRAINT chk_attachments_status CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    CONSTRAINT chk_attachments_secondary_status CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
    CONSTRAINT chk_attachments_secondary_kind CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic'))
);",
    r"
CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat
    ON attachments (tenant_id, chat_id)
    WHERE deleted_at IS NULL;",
    r"
CREATE INDEX IF NOT EXISTS idx_attachments_cleanup
    ON attachments (cleanup_status)
    WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL;",
    r"
CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload
    ON attachments (status, cleanup_status, deleted_at, updated_at);",
    r"
CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL,
    message_id UUID NOT NULL,
    attachment_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (chat_id, message_id, attachment_id),
    CONSTRAINT fk_ma_message FOREIGN KEY (message_id, chat_id) REFERENCES messages(id, chat_id) ON DELETE CASCADE,
    CONSTRAINT fk_ma_attachment FOREIGN KEY (attachment_id, chat_id) REFERENCES attachments(id, chat_id) ON DELETE CASCADE
);",
    r"
CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat
    ON message_attachments (tenant_id, chat_id);",
    r"
CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment
    ON message_attachments (attachment_id, chat_id);",
    r"
CREATE TABLE IF NOT EXISTS thread_summaries (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL UNIQUE REFERENCES chats(id) ON DELETE CASCADE,
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at TIMESTAMPTZ NOT NULL,
    summarized_up_to_message_id UUID NOT NULL,
    token_estimate INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);",
    r"
CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    chat_id UUID NOT NULL,
    vector_store_id VARCHAR(128),
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT uq_chat_vector_stores_tenant_chat UNIQUE (tenant_id, chat_id)
);",
    r"
CREATE TABLE IF NOT EXISTS quota_usage (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    user_id UUID NOT NULL,
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
    updated_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT uq_quota_usage UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
);",
    r"
CREATE INDEX IF NOT EXISTS idx_quota_usage_lookup
    ON quota_usage (tenant_id, user_id, period_type, period_start, bucket);",
    r"
CREATE TABLE IF NOT EXISTS message_reactions (
    id UUID PRIMARY KEY,
    message_id UUID NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id UUID NOT NULL,
    tenant_id UUID NOT NULL,
    reaction VARCHAR(16) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT uq_message_reactions_message_user UNIQUE (message_id, user_id),
    CONSTRAINT chk_message_reactions_value CHECK (reaction IN ('like', 'dislike'))
);",
];

const SQLITE_STATEMENTS: &[&str] = &[
    r"
CREATE TABLE IF NOT EXISTS chats (
    id BLOB PRIMARY KEY NOT NULL CHECK (length(id) = 16),
    tenant_id BLOB NOT NULL CHECK (length(tenant_id) = 16),
    user_id BLOB NOT NULL CHECK (length(user_id) = 16),
    model TEXT NOT NULL,
    title TEXT,
    is_temporary INTEGER NOT NULL DEFAULT 0 CHECK (is_temporary IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    deleted_at TEXT
);",
    r"
CREATE TABLE IF NOT EXISTS messages (
    id BLOB PRIMARY KEY NOT NULL CHECK (length(id) = 16),
    tenant_id BLOB NOT NULL CHECK (length(tenant_id) = 16),
    chat_id BLOB NOT NULL REFERENCES chats(id) ON DELETE CASCADE CHECK (length(chat_id) = 16),
    request_id BLOB CHECK (request_id IS NULL OR length(request_id) = 16),
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    content_type TEXT NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    provider_response_id TEXT,
    request_kind TEXT,
    features_used TEXT NOT NULL DEFAULT '[]',
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_input_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    model TEXT,
    is_compressed INTEGER NOT NULL DEFAULT 0 CHECK (is_compressed IN (0, 1)),
    created_at TEXT NOT NULL,
    deleted_at TEXT,
    CONSTRAINT uq_messages_id_chat UNIQUE (id, chat_id)
);",
    r"
CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role
    ON messages (chat_id, request_id, role)
    WHERE request_id IS NOT NULL AND deleted_at IS NULL;",
    r"
CREATE INDEX IF NOT EXISTS idx_messages_chat_created
    ON messages (chat_id, created_at)
    WHERE deleted_at IS NULL;",
    r"
CREATE TABLE IF NOT EXISTS chat_turns (
    id BLOB PRIMARY KEY NOT NULL CHECK (length(id) = 16),
    tenant_id BLOB NOT NULL CHECK (length(tenant_id) = 16),
    chat_id BLOB NOT NULL REFERENCES chats(id) ON DELETE CASCADE CHECK (length(chat_id) = 16),
    request_id BLOB NOT NULL CHECK (length(request_id) = 16),
    requester_type TEXT NOT NULL,
    requester_user_id BLOB CHECK (requester_user_id IS NULL OR length(requester_user_id) = 16),
    state TEXT NOT NULL,
    provider_name TEXT,
    provider_response_id TEXT,
    assistant_message_id BLOB CHECK (assistant_message_id IS NULL OR length(assistant_message_id) = 16),
    error_code TEXT,
    reserve_tokens INTEGER,
    max_output_tokens_applied INTEGER,
    reserved_credits_micro INTEGER,
    policy_version_applied INTEGER,
    effective_model TEXT,
    minimal_generation_floor_applied INTEGER,
    error_detail TEXT,
    deleted_at TEXT,
    replaced_by_request_id BLOB CHECK (replaced_by_request_id IS NULL OR length(replaced_by_request_id) = 16),
    started_at TEXT NOT NULL,
    last_progress_at TEXT,
    web_search_enabled INTEGER NOT NULL DEFAULT 0 CHECK (web_search_enabled IN (0, 1)),
    web_search_completed_count INTEGER NOT NULL DEFAULT 0,
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0,
    file_search_completed_count INTEGER NOT NULL DEFAULT 0,
    completed_at TEXT,
    updated_at TEXT NOT NULL,
    CONSTRAINT uq_chat_turns_chat_request UNIQUE (chat_id, request_id),
    CONSTRAINT chk_chat_turns_requester_type CHECK (requester_type IN ('user', 'system')),
    CONSTRAINT chk_chat_turns_state CHECK (state IN ('running', 'completed', 'failed', 'cancelled'))
);",
    r"
CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started
    ON chat_turns (chat_id, started_at DESC)
    WHERE deleted_at IS NULL;",
    r"
CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_one_running
    ON chat_turns (chat_id)
    WHERE state = 'running' AND deleted_at IS NULL;",
    r"
CREATE INDEX IF NOT EXISTS idx_chat_turns_progress
    ON chat_turns (last_progress_at)
    WHERE state = 'running' AND deleted_at IS NULL;",
    r"
CREATE TABLE IF NOT EXISTS attachments (
    id BLOB PRIMARY KEY NOT NULL CHECK (length(id) = 16),
    tenant_id BLOB NOT NULL CHECK (length(tenant_id) = 16),
    chat_id BLOB NOT NULL REFERENCES chats(id) ON DELETE CASCADE CHECK (length(chat_id) = 16),
    uploaded_by_user_id BLOB NOT NULL CHECK (length(uploaded_by_user_id) = 16),
    filename TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    storage_backend TEXT NOT NULL DEFAULT 'azure',
    provider_file_id TEXT,
    status TEXT NOT NULL,
    error_code TEXT,
    attachment_kind TEXT NOT NULL,
    for_file_search INTEGER NOT NULL DEFAULT 0 CHECK (for_file_search IN (0, 1)),
    for_code_interpreter INTEGER NOT NULL DEFAULT 0 CHECK (for_code_interpreter IN (0, 1)),
    doc_summary TEXT,
    img_thumbnail BLOB,
    img_thumbnail_width INTEGER,
    img_thumbnail_height INTEGER,
    summary_model TEXT,
    summary_updated_at TEXT,
    cleanup_status TEXT,
    cleanup_attempts INTEGER NOT NULL DEFAULT 0,
    last_cleanup_error TEXT,
    cleanup_updated_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at TEXT,
    secondary_file_id TEXT,
    secondary_status TEXT NOT NULL DEFAULT 'not_attempted',
    secondary_provider_kind TEXT,
    CONSTRAINT uq_attachments_id_chat UNIQUE (id, chat_id),
    CONSTRAINT chk_attachments_kind CHECK (attachment_kind IN ('document', 'image')),
    CONSTRAINT chk_attachments_status CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    CONSTRAINT chk_attachments_secondary_status CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
    CONSTRAINT chk_attachments_secondary_kind CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic'))
);",
    r"
CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat
    ON attachments (tenant_id, chat_id)
    WHERE deleted_at IS NULL;",
    r"
CREATE INDEX IF NOT EXISTS idx_attachments_cleanup
    ON attachments (cleanup_status)
    WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL;",
    r"
CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload
    ON attachments (status, cleanup_status, deleted_at, updated_at);",
    r"
CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id BLOB NOT NULL CHECK (length(tenant_id) = 16),
    chat_id BLOB NOT NULL CHECK (length(chat_id) = 16),
    message_id BLOB NOT NULL CHECK (length(message_id) = 16),
    attachment_id BLOB NOT NULL CHECK (length(attachment_id) = 16),
    created_at TEXT NOT NULL,
    PRIMARY KEY (chat_id, message_id, attachment_id),
    CONSTRAINT fk_ma_message FOREIGN KEY (message_id, chat_id) REFERENCES messages(id, chat_id) ON DELETE CASCADE,
    CONSTRAINT fk_ma_attachment FOREIGN KEY (attachment_id, chat_id) REFERENCES attachments(id, chat_id) ON DELETE CASCADE
);",
    r"
CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat
    ON message_attachments (tenant_id, chat_id);",
    r"
CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment
    ON message_attachments (attachment_id, chat_id);",
    r"
CREATE TABLE IF NOT EXISTS thread_summaries (
    id BLOB PRIMARY KEY NOT NULL CHECK (length(id) = 16),
    tenant_id BLOB NOT NULL CHECK (length(tenant_id) = 16),
    chat_id BLOB NOT NULL UNIQUE REFERENCES chats(id) ON DELETE CASCADE CHECK (length(chat_id) = 16),
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at TEXT NOT NULL,
    summarized_up_to_message_id BLOB NOT NULL CHECK (length(summarized_up_to_message_id) = 16),
    token_estimate INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);",
    r"
CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id BLOB PRIMARY KEY NOT NULL CHECK (length(id) = 16),
    tenant_id BLOB NOT NULL CHECK (length(tenant_id) = 16),
    chat_id BLOB NOT NULL CHECK (length(chat_id) = 16),
    vector_store_id TEXT,
    provider TEXT NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at TEXT NOT NULL,
    CONSTRAINT uq_chat_vector_stores_tenant_chat UNIQUE (tenant_id, chat_id)
);",
    r"
CREATE TABLE IF NOT EXISTS quota_usage (
    id BLOB PRIMARY KEY NOT NULL CHECK (length(id) = 16),
    tenant_id BLOB NOT NULL CHECK (length(tenant_id) = 16),
    user_id BLOB NOT NULL CHECK (length(user_id) = 16),
    period_type TEXT NOT NULL,
    period_start TEXT NOT NULL,
    bucket TEXT NOT NULL,
    spent_credits_micro INTEGER NOT NULL DEFAULT 0,
    reserved_credits_micro INTEGER NOT NULL DEFAULT 0,
    calls INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    file_search_calls INTEGER NOT NULL DEFAULT 0,
    web_search_calls INTEGER NOT NULL DEFAULT 0,
    code_interpreter_calls INTEGER NOT NULL DEFAULT 0,
    rag_retrieval_calls INTEGER NOT NULL DEFAULT 0,
    image_inputs INTEGER NOT NULL DEFAULT 0,
    image_upload_bytes INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    CONSTRAINT uq_quota_usage UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
);",
    r"
CREATE INDEX IF NOT EXISTS idx_quota_usage_lookup
    ON quota_usage (tenant_id, user_id, period_type, period_start, bucket);",
    r"
CREATE TABLE IF NOT EXISTS message_reactions (
    id BLOB PRIMARY KEY NOT NULL CHECK (length(id) = 16),
    message_id BLOB NOT NULL REFERENCES messages(id) ON DELETE CASCADE CHECK (length(message_id) = 16),
    user_id BLOB NOT NULL CHECK (length(user_id) = 16),
    tenant_id BLOB NOT NULL CHECK (length(tenant_id) = 16),
    reaction TEXT NOT NULL,
    created_at TEXT NOT NULL,
    CONSTRAINT uq_message_reactions_message_user UNIQUE (message_id, user_id),
    CONSTRAINT chk_message_reactions_value CHECK (reaction IN ('like', 'dislike'))
);",
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        let statements = match backend {
            sea_orm::DatabaseBackend::Postgres => PG_STATEMENTS,
            sea_orm::DatabaseBackend::Sqlite => SQLITE_STATEMENTS,
            _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        };
        for sql in statements {
            conn.execute_unprepared(sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        for table in [
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
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table};"))
                .await?;
        }
        Ok(())
    }
}
