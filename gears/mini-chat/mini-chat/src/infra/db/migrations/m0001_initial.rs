//! Initial mini-chat schema (DESIGN section 3.7), PostgreSQL and SQLite variants.
//!
//! SQLite: UUID columns are `BLOB` (16 raw bytes), timestamps/dates/JSON are `TEXT`,
//! booleans are `INTEGER CHECK (x IN (0,1))`. PostgreSQL uses the native types.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend};

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Dialect-specific column type spellings.
#[derive(Clone, Copy)]
struct Dialect {
    pg: bool,
}

impl Dialect {
    fn uuid(self) -> &'static str {
        if self.pg { "UUID" } else { "BLOB" }
    }
    fn ts(self) -> &'static str {
        if self.pg { "TIMESTAMPTZ" } else { "TEXT" }
    }
    fn date(self) -> &'static str {
        if self.pg { "DATE" } else { "TEXT" }
    }
    fn bytes(self) -> &'static str {
        if self.pg { "BYTEA" } else { "BLOB" }
    }
    fn now(self) -> &'static str {
        // SQLite: RFC 3339 text with exactly nine fractional digits (milliseconds, padded with
        // `000001` ns), the shape `infra::db::ts::normalize` gives written values, so that
        // default-stamped rows order and compare correctly (lexically) against written ones.
        if self.pg {
            "now()"
        } else {
            "(strftime('%Y-%m-%dT%H:%M:%f', 'now') || '000001Z')"
        }
    }
    /// `NOT NULL` JSON column defaulting to an empty array.
    fn json_array(self, name: &str) -> String {
        if self.pg {
            format!("{name} JSONB NOT NULL DEFAULT '[]'::jsonb")
        } else {
            format!("{name} TEXT NOT NULL DEFAULT '[]'")
        }
    }
    /// `NOT NULL` boolean column with a default.
    fn boolean(self, name: &str, default: bool) -> String {
        if self.pg {
            format!(
                "{name} BOOLEAN NOT NULL DEFAULT {}",
                if default { "TRUE" } else { "FALSE" }
            )
        } else {
            format!(
                "{name} INTEGER NOT NULL DEFAULT {} CHECK ({name} IN (0,1))",
                i32::from(default)
            )
        }
    }
}

fn statements(d: Dialect) -> Vec<String> {
    let (uuid, ts, date, bytes, now) = (d.uuid(), d.ts(), d.date(), d.bytes(), d.now());
    let is_temporary = d.boolean("is_temporary", false);
    let features_used = d.json_array("features_used");
    let is_compressed = d.boolean("is_compressed", false);
    let web_search_enabled = d.boolean("web_search_enabled", false);
    let for_file_search = d.boolean("for_file_search", false);
    let for_code_interpreter = d.boolean("for_code_interpreter", false);

    vec![
        // ---- chats -------------------------------------------------------------------------
        format!(
            "CREATE TABLE chats (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                user_id {uuid} NOT NULL,
                model VARCHAR(1024) NOT NULL,
                title VARCHAR(255) NULL,
                {is_temporary},
                created_at {ts} NOT NULL,
                updated_at {ts} NOT NULL,
                deleted_at {ts} NULL
            )"
        ),
        "CREATE INDEX idx_chats_tenant_user_updated ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL".to_owned(),
        // ---- messages ----------------------------------------------------------------------
        format!(
            "CREATE TABLE messages (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
                request_id {uuid} NULL,
                role VARCHAR(16) NOT NULL,
                content TEXT NOT NULL,
                content_type VARCHAR(32) NOT NULL DEFAULT 'text',
                token_estimate INTEGER NOT NULL DEFAULT 0,
                provider_response_id VARCHAR(128) NULL,
                request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
                {features_used},
                input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
                output_tokens BIGINT NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
                cache_read_input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cache_read_input_tokens >= 0),
                cache_write_input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cache_write_input_tokens >= 0),
                reasoning_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reasoning_tokens >= 0),
                model VARCHAR(1024) NULL,
                {is_compressed},
                created_at {ts} NOT NULL,
                deleted_at {ts} NULL
            )"
        ),
        "CREATE UNIQUE INDEX uq_messages_chat_request_role ON messages (chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL".to_owned(),
        "CREATE INDEX idx_messages_chat_created ON messages (chat_id, created_at) WHERE deleted_at IS NULL".to_owned(),
        "CREATE UNIQUE INDEX uq_messages_id_chat ON messages (id, chat_id)".to_owned(),
        // ---- chat_turns --------------------------------------------------------------------
        format!(
            "CREATE TABLE chat_turns (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
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
                {web_search_enabled},
                web_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (web_search_completed_count >= 0),
                code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (code_interpreter_completed_count >= 0),
                file_search_completed_count INTEGER NOT NULL DEFAULT 0 CHECK (file_search_completed_count >= 0),
                completed_at {ts} NULL,
                updated_at {ts} NOT NULL DEFAULT {now}
            )"
        ),
        "CREATE UNIQUE INDEX uq_chat_turns_chat_request ON chat_turns (chat_id, request_id)".to_owned(),
        "CREATE UNIQUE INDEX uq_chat_turns_running ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL".to_owned(),
        "CREATE INDEX idx_chat_turns_chat_started ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL".to_owned(),
        "CREATE INDEX idx_chat_turns_running_progress ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL".to_owned(),
        // ---- attachments -------------------------------------------------------------------
        format!(
            "CREATE TABLE attachments (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
                uploaded_by_user_id {uuid} NOT NULL,
                filename VARCHAR(255) NOT NULL,
                content_type VARCHAR(128) NOT NULL,
                size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0),
                storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
                provider_file_id VARCHAR(128) NULL,
                status VARCHAR(16) NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
                error_code VARCHAR(64) NULL,
                attachment_kind VARCHAR(16) NOT NULL CHECK (attachment_kind IN ('document', 'image')),
                {for_file_search},
                {for_code_interpreter},
                doc_summary TEXT NULL,
                img_thumbnail {bytes} NULL,
                img_thumbnail_width INTEGER NULL,
                img_thumbnail_height INTEGER NULL,
                summary_model VARCHAR(1024) NULL,
                summary_updated_at {ts} NULL,
                cleanup_status VARCHAR(16) NULL,
                cleanup_attempts INTEGER NOT NULL DEFAULT 0 CHECK (cleanup_attempts >= 0),
                last_cleanup_error TEXT NULL,
                cleanup_updated_at {ts} NULL,
                created_at {ts} NOT NULL,
                updated_at {ts} NOT NULL DEFAULT {now},
                deleted_at {ts} NULL,
                secondary_file_id VARCHAR(128) NULL,
                secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted'
                    CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
                secondary_provider_kind VARCHAR(32) NULL
                    CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic'))
            )"
        ),
        "CREATE INDEX idx_attachments_tenant_chat ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL".to_owned(),
        "CREATE INDEX idx_attachments_cleanup_status ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL".to_owned(),
        "CREATE INDEX idx_attachments_stale_upload ON attachments (status, cleanup_status, deleted_at, updated_at)".to_owned(),
        "CREATE UNIQUE INDEX uq_attachments_id_chat ON attachments (id, chat_id)".to_owned(),
        // ---- message_attachments -----------------------------------------------------------
        format!(
            "CREATE TABLE message_attachments (
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL,
                message_id {uuid} NOT NULL,
                attachment_id {uuid} NOT NULL,
                created_at {ts} NOT NULL,
                PRIMARY KEY (chat_id, message_id, attachment_id),
                FOREIGN KEY (message_id, chat_id) REFERENCES messages (id, chat_id) ON DELETE CASCADE,
                FOREIGN KEY (attachment_id, chat_id) REFERENCES attachments (id, chat_id) ON DELETE CASCADE
            )"
        ),
        "CREATE INDEX idx_message_attachments_tenant_chat ON message_attachments (tenant_id, chat_id)".to_owned(),
        "CREATE INDEX idx_message_attachments_attachment_chat ON message_attachments (attachment_id, chat_id)".to_owned(),
        // ---- thread_summaries --------------------------------------------------------------
        format!(
            "CREATE TABLE thread_summaries (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
                summary_text TEXT NOT NULL,
                summarized_up_to_created_at {ts} NOT NULL,
                summarized_up_to_message_id {uuid} NOT NULL,
                token_estimate INTEGER NOT NULL DEFAULT 0,
                created_at {ts} NOT NULL,
                updated_at {ts} NOT NULL
            )"
        ),
        "CREATE UNIQUE INDEX uq_thread_summaries_chat ON thread_summaries (chat_id)".to_owned(),
        // ---- chat_vector_stores ------------------------------------------------------------
        format!(
            "CREATE TABLE chat_vector_stores (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL,
                vector_store_id VARCHAR(128) NULL,
                provider VARCHAR(128) NOT NULL,
                file_count INTEGER NOT NULL DEFAULT 0 CHECK (file_count >= 0),
                created_at {ts} NOT NULL
            )"
        ),
        "CREATE UNIQUE INDEX uq_chat_vector_stores_tenant_chat ON chat_vector_stores (tenant_id, chat_id)".to_owned(),
        // ---- quota_usage -------------------------------------------------------------------
        format!(
            "CREATE TABLE quota_usage (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                user_id {uuid} NOT NULL,
                period_type VARCHAR(16) NOT NULL,
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
                updated_at {ts} NOT NULL DEFAULT {now}
            )"
        ),
        "CREATE UNIQUE INDEX uq_quota_usage_bucket ON quota_usage (tenant_id, user_id, period_type, period_start, bucket)".to_owned(),
        // ---- message_reactions -------------------------------------------------------------
        format!(
            "CREATE TABLE message_reactions (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                message_id {uuid} NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
                user_id {uuid} NOT NULL,
                reaction VARCHAR(16) NOT NULL CHECK (reaction IN ('like', 'dislike')),
                created_at {ts} NOT NULL
            )"
        ),
        "CREATE UNIQUE INDEX uq_message_reactions_message_user ON message_reactions (message_id, user_id)".to_owned(),
    ]
}

/// Tables in reverse dependency order, for `down`.
const TABLES_DROP_ORDER: [&str; 9] = [
    "message_reactions",
    "quota_usage",
    "chat_vector_stores",
    "thread_summaries",
    "message_attachments",
    "attachments",
    "chat_turns",
    "messages",
    "chats",
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let dialect = match manager.get_database_backend() {
            DatabaseBackend::Postgres => Dialect { pg: true },
            DatabaseBackend::Sqlite => Dialect { pg: false },
            _ => return Err(DbErr::Custom("mini-chat: MySQL is not supported".into())),
        };
        let conn = manager.get_connection();
        for sql in statements(dialect) {
            conn.execute_unprepared(&sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        for table in TABLES_DROP_ORDER {
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table}"))
                .await?;
        }
        Ok(())
    }
}
