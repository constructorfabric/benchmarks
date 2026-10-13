//! Initial schema: every table of DESIGN section 3.7, for PostgreSQL and SQLite.
//!
//! Raw per-backend SQL keeps `CHECK` constraints, partial indexes and composite foreign keys
//! verbatim. UUID columns are `UUID` on PostgreSQL and `TEXT` on SQLite, where the values are
//! written as 16-byte blobs. The cross-column `CHECK`s of `chat_turns` and the
//! `attachments.cleanup_status` value set are deliberately not enforced by the database
//! (ADR-0010); the repositories keep those invariants. MySQL is not supported.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend};

const MYSQL_NOT_SUPPORTED: &str =
    "mini-chat migrations: MySQL is not supported (this migration set targets PostgreSQL/SQLite)";

const TABLES_IN_DROP_ORDER: [&str; 9] = [
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

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Column types and literals that differ between the supported backends.
struct Dialect {
    uuid: &'static str,
    timestamp: &'static str,
    date: &'static str,
    json: &'static str,
    json_empty_array: &'static str,
    bytes: &'static str,
    boolean_false: &'static str,
}

impl Dialect {
    const POSTGRES: Self = Self {
        uuid: "UUID",
        timestamp: "TIMESTAMPTZ",
        date: "DATE",
        json: "JSONB",
        json_empty_array: "'[]'::jsonb",
        bytes: "BYTEA",
        boolean_false: "FALSE",
    };

    const SQLITE: Self = Self {
        uuid: "TEXT",
        timestamp: "TEXT",
        date: "TEXT",
        json: "TEXT",
        json_empty_array: "'[]'",
        bytes: "BLOB",
        boolean_false: "0",
    };

    fn for_backend(backend: DatabaseBackend) -> Result<Self, DbErr> {
        match backend {
            DatabaseBackend::Postgres => Ok(Self::POSTGRES),
            DatabaseBackend::Sqlite => Ok(Self::SQLITE),
            _ => Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        }
    }

    fn statements(&self) -> Vec<String> {
        let mut statements = Vec::new();
        statements.extend(self.chats());
        statements.extend(self.messages());
        statements.extend(self.chat_turns());
        statements.extend(self.attachments());
        statements.extend(self.message_attachments());
        statements.extend(self.thread_summaries());
        statements.extend(self.chat_vector_stores());
        statements.extend(self.quota_usage());
        statements.extend(self.message_reactions());
        statements
    }

    fn chats(&self) -> Vec<String> {
        let Self {
            uuid,
            timestamp: ts,
            boolean_false: no,
            ..
        } = self;
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS chats (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    user_id {uuid} NOT NULL,
    model VARCHAR(1024) NOT NULL,
    title VARCHAR(255) NULL,
    is_temporary BOOLEAN NOT NULL DEFAULT {no},
    created_at {ts} NOT NULL,
    updated_at {ts} NOT NULL,
    deleted_at {ts} NULL
)"
            ),
            "CREATE INDEX IF NOT EXISTS idx_chats_owner_updated \
             ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL"
                .to_owned(),
        ]
    }

    fn messages(&self) -> Vec<String> {
        let Self {
            uuid,
            timestamp: ts,
            json,
            json_empty_array: empty_array,
            boolean_false: no,
            ..
        } = self;
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS messages (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    request_id {uuid} NULL,
    role VARCHAR(16) NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL DEFAULT 'text',
    token_estimate INTEGER NOT NULL DEFAULT 0,
    provider_response_id VARCHAR(128) NULL,
    request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
    features_used {json} NOT NULL DEFAULT {empty_array},
    input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens BIGINT NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cache_read_input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cache_read_input_tokens >= 0),
    cache_write_input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cache_write_input_tokens >= 0),
    reasoning_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reasoning_tokens >= 0),
    model VARCHAR(1024) NULL,
    is_compressed BOOLEAN NOT NULL DEFAULT {no},
    created_at {ts} NOT NULL,
    deleted_at {ts} NULL,
    CONSTRAINT fk_messages_chat FOREIGN KEY (chat_id) REFERENCES chats (id) ON DELETE CASCADE,
    CONSTRAINT uq_messages_id_chat UNIQUE (id, chat_id)
)"
            ),
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_request_role \
             ON messages (chat_id, request_id, role) \
             WHERE request_id IS NOT NULL AND deleted_at IS NULL"
                .to_owned(),
            "CREATE INDEX IF NOT EXISTS idx_messages_chat_created \
             ON messages (chat_id, created_at) WHERE deleted_at IS NULL"
                .to_owned(),
        ]
    }

    fn chat_turns(&self) -> Vec<String> {
        let Self {
            uuid,
            timestamp: ts,
            boolean_false: no,
            ..
        } = self;
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS chat_turns (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
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
    web_search_enabled BOOLEAN NOT NULL DEFAULT {no},
    web_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (web_search_completed_count >= 0),
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0
        CHECK (code_interpreter_completed_count >= 0),
    file_search_completed_count INTEGER NOT NULL DEFAULT 0
        CHECK (file_search_completed_count >= 0),
    completed_at {ts} NULL,
    updated_at {ts} NOT NULL,
    CONSTRAINT fk_chat_turns_chat FOREIGN KEY (chat_id) REFERENCES chats (id) ON DELETE CASCADE,
    CONSTRAINT uq_chat_turns_chat_request UNIQUE (chat_id, request_id)
)"
            ),
            "CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started \
             ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL"
                .to_owned(),
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_one_running \
             ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL"
                .to_owned(),
            "CREATE INDEX IF NOT EXISTS idx_chat_turns_orphan_scan \
             ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL"
                .to_owned(),
        ]
    }

    fn attachments(&self) -> Vec<String> {
        let Self {
            uuid,
            timestamp: ts,
            bytes,
            boolean_false: no,
            ..
        } = self;
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS attachments (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    uploaded_by_user_id {uuid} NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128) NULL,
    size_bytes BIGINT NULL,
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128) NULL,
    status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    error_code VARCHAR(64) NULL,
    attachment_kind VARCHAR(16) NOT NULL CHECK (attachment_kind IN ('document', 'image')),
    for_file_search BOOLEAN NOT NULL DEFAULT {no},
    for_code_interpreter BOOLEAN NOT NULL DEFAULT {no},
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
    CONSTRAINT fk_attachments_chat FOREIGN KEY (chat_id) REFERENCES chats (id) ON DELETE CASCADE,
    CONSTRAINT uq_attachments_id_chat UNIQUE (id, chat_id)
)"
            ),
            "CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat \
             ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL"
                .to_owned(),
            "CREATE INDEX IF NOT EXISTS idx_attachments_cleanup_status \
             ON attachments (cleanup_status) \
             WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL"
                .to_owned(),
            // A plain index: the upload reaper binds the status values as parameters and SQLite
            // uses a partial index only when the query repeats its WHERE literally.
            "CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload \
             ON attachments (status, cleanup_status, deleted_at, updated_at)"
                .to_owned(),
        ]
    }

    fn message_attachments(&self) -> Vec<String> {
        let Self {
            uuid,
            timestamp: ts,
            ..
        } = self;
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS message_attachments (
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    message_id {uuid} NOT NULL,
    attachment_id {uuid} NOT NULL,
    created_at {ts} NOT NULL,
    PRIMARY KEY (chat_id, message_id, attachment_id),
    CONSTRAINT fk_message_attachments_message FOREIGN KEY (message_id, chat_id)
        REFERENCES messages (id, chat_id) ON DELETE CASCADE,
    CONSTRAINT fk_message_attachments_attachment FOREIGN KEY (attachment_id, chat_id)
        REFERENCES attachments (id, chat_id) ON DELETE CASCADE
)"
            ),
            "CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat \
             ON message_attachments (tenant_id, chat_id)"
                .to_owned(),
            "CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment_chat \
             ON message_attachments (attachment_id, chat_id)"
                .to_owned(),
        ]
    }

    fn thread_summaries(&self) -> Vec<String> {
        let Self {
            uuid,
            timestamp: ts,
            ..
        } = self;
        vec![format!(
            "CREATE TABLE IF NOT EXISTS thread_summaries (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    summary_text TEXT NOT NULL,
    summarized_up_to_created_at {ts} NOT NULL,
    summarized_up_to_message_id {uuid} NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    created_at {ts} NOT NULL,
    updated_at {ts} NOT NULL,
    CONSTRAINT uq_thread_summaries_chat UNIQUE (chat_id),
    CONSTRAINT fk_thread_summaries_chat FOREIGN KEY (chat_id) REFERENCES chats (id)
        ON DELETE CASCADE
)"
        )]
    }

    fn chat_vector_stores(&self) -> Vec<String> {
        let Self {
            uuid,
            timestamp: ts,
            ..
        } = self;
        vec![format!(
            "CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    vector_store_id VARCHAR(128) NULL,
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at {ts} NOT NULL,
    CONSTRAINT uq_chat_vector_stores_tenant_chat UNIQUE (tenant_id, chat_id)
)"
        )]
    }

    fn quota_usage(&self) -> Vec<String> {
        let Self {
            uuid,
            timestamp: ts,
            date,
            ..
        } = self;
        vec![format!(
            "CREATE TABLE IF NOT EXISTS quota_usage (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    user_id {uuid} NOT NULL,
    period_type VARCHAR(16) NOT NULL CHECK (period_type IN ('daily', 'monthly')),
    period_start {date} NOT NULL,
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
    updated_at {ts} NOT NULL,
    CONSTRAINT uq_quota_usage_bucket
        UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
)"
        )]
    }

    fn message_reactions(&self) -> Vec<String> {
        let Self {
            uuid,
            timestamp: ts,
            ..
        } = self;
        vec![format!(
            "CREATE TABLE IF NOT EXISTS message_reactions (
    id {uuid} PRIMARY KEY NOT NULL,
    message_id {uuid} NOT NULL,
    user_id {uuid} NOT NULL,
    tenant_id {uuid} NOT NULL,
    reaction VARCHAR(16) NOT NULL CHECK (reaction IN ('like', 'dislike')),
    created_at {ts} NOT NULL,
    CONSTRAINT uq_message_reactions_message_user UNIQUE (message_id, user_id),
    CONSTRAINT fk_message_reactions_message FOREIGN KEY (message_id) REFERENCES messages (id)
        ON DELETE CASCADE
)"
        )]
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let dialect = Dialect::for_backend(manager.get_database_backend())?;
        let conn = manager.get_connection();
        for statement in dialect.statements() {
            conn.execute_unprepared(&statement).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        Dialect::for_backend(manager.get_database_backend())?;
        let conn = manager.get_connection();
        for table in TABLES_IN_DROP_ORDER {
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table}"))
                .await?;
        }
        Ok(())
    }
}
