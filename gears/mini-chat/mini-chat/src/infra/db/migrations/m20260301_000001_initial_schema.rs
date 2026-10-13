//! P1 schema (DESIGN 3.7): `chats`, `messages`, `chat_turns`, `attachments`,
//! `message_attachments`, `thread_summaries`, `chat_vector_stores`,
//! `quota_usage`, `message_reactions`.
//!
//! One statement list serves both engines; the dialect only swaps column
//! types and literals (PostgreSQL: `UUID`, `TIMESTAMPTZ`, `JSONB`, `BYTEA`;
//! SQLite: UUIDs declared `TEXT` but written as 16-byte blobs, timestamps and
//! JSON as `TEXT`, `BLOB`). Raw SQL is used so that partial indexes, composite
//! foreign keys and CHECKs are preserved verbatim. MySQL is not supported.
//!
//! The cross-column `chat_turns` CHECKs (`completed_at` / `last_progress_at`
//! vs `state`) and the `attachments.cleanup_status` value CHECK are
//! intentionally not enforced by the database (ADR-0010): the repositories keep
//! those invariants.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend};

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Column types and literals that differ between the engines.
struct Dialect {
    uuid: &'static str,
    timestamp: &'static str,
    json: &'static str,
    bytes: &'static str,
    empty_json_array: &'static str,
    now: &'static str,
}

const POSTGRES: Dialect = Dialect {
    uuid: "UUID",
    timestamp: "TIMESTAMPTZ",
    json: "JSONB",
    bytes: "BYTEA",
    empty_json_array: "'[]'::jsonb",
    now: "now()",
};

const SQLITE: Dialect = Dialect {
    uuid: "TEXT",
    timestamp: "TEXT",
    json: "TEXT",
    bytes: "BLOB",
    empty_json_array: "'[]'",
    now: "CURRENT_TIMESTAMP",
};

/// Tables in creation order (children after parents).
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

fn dialect(backend: DatabaseBackend) -> Result<Dialect, DbErr> {
    match backend {
        DatabaseBackend::Postgres => Ok(POSTGRES),
        DatabaseBackend::Sqlite => Ok(SQLITE),
        _ => Err(DbErr::Custom(
            "mini-chat migrations: only PostgreSQL and SQLite are supported".to_owned(),
        )),
    }
}

fn statements(d: &Dialect) -> Vec<String> {
    let mut all = chat_statements(d);
    all.extend(attachment_statements(d));
    all.extend(usage_statements(d));
    all
}

/// `chats`, `messages`, `chat_turns`.
fn chat_statements(d: &Dialect) -> Vec<String> {
    let Dialect {
        uuid,
        timestamp: ts,
        json,
        empty_json_array,
        ..
    } = d;
    vec![
        // ── chats ───────────────────────────────────────────────────────────
        format!(
            "CREATE TABLE chats (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                user_id {uuid} NOT NULL,
                model VARCHAR(1024) NOT NULL,
                title VARCHAR(255),
                is_temporary BOOLEAN NOT NULL DEFAULT FALSE,
                created_at {ts} NOT NULL,
                updated_at {ts} NOT NULL,
                deleted_at {ts}
            )"
        ),
        "CREATE INDEX idx_chats_list ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL".to_owned(),
        // ── messages ────────────────────────────────────────────────────────
        format!(
            "CREATE TABLE messages (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
                request_id {uuid},
                role VARCHAR(16) NOT NULL,
                content TEXT NOT NULL,
                content_type VARCHAR(32) NOT NULL,
                token_estimate INTEGER NOT NULL DEFAULT 0,
                provider_response_id VARCHAR(128),
                request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
                features_used {json} NOT NULL DEFAULT {empty_json_array},
                input_tokens BIGINT NOT NULL DEFAULT 0,
                output_tokens BIGINT NOT NULL DEFAULT 0,
                cache_read_input_tokens BIGINT NOT NULL DEFAULT 0,
                cache_write_input_tokens BIGINT NOT NULL DEFAULT 0,
                reasoning_tokens BIGINT NOT NULL DEFAULT 0,
                model VARCHAR(1024),
                is_compressed BOOLEAN NOT NULL DEFAULT FALSE,
                created_at {ts} NOT NULL,
                deleted_at {ts},
                CONSTRAINT uq_messages_id_chat UNIQUE (id, chat_id)
            )"
        ),
        "CREATE UNIQUE INDEX uq_messages_request ON messages (chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL".to_owned(),
        "CREATE INDEX idx_messages_chrono ON messages (chat_id, created_at) WHERE deleted_at IS NULL".to_owned(),
        // ── chat_turns ──────────────────────────────────────────────────────
        format!(
            "CREATE TABLE chat_turns (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
                request_id {uuid} NOT NULL,
                requester_type VARCHAR(16) NOT NULL
                    CONSTRAINT ck_chat_turns_requester_type CHECK (requester_type IN ('user', 'system')),
                requester_user_id {uuid},
                state VARCHAR(16) NOT NULL
                    CONSTRAINT ck_chat_turns_state CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
                provider_name VARCHAR(128),
                provider_response_id VARCHAR(128),
                assistant_message_id {uuid},
                error_code VARCHAR(64),
                reserve_tokens BIGINT,
                max_output_tokens_applied INTEGER,
                reserved_credits_micro BIGINT,
                policy_version_applied BIGINT,
                effective_model VARCHAR(1024),
                minimal_generation_floor_applied INTEGER,
                error_detail TEXT,
                deleted_at {ts},
                replaced_by_request_id {uuid},
                started_at {ts} NOT NULL,
                last_progress_at {ts},
                web_search_enabled BOOLEAN NOT NULL DEFAULT FALSE,
                web_search_completed_count INTEGER NOT NULL DEFAULT 0,
                code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0,
                file_search_completed_count INTEGER NOT NULL DEFAULT 0,
                completed_at {ts},
                updated_at {ts},
                CONSTRAINT uq_chat_turns_request UNIQUE (chat_id, request_id)
            )"
        ),
        "CREATE UNIQUE INDEX uq_chat_turns_running ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL".to_owned(),
        "CREATE INDEX idx_chat_turns_latest ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL".to_owned(),
        "CREATE INDEX idx_chat_turns_orphan ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL".to_owned(),
    ]
}

/// `attachments`, `message_attachments`, `thread_summaries`.
fn attachment_statements(d: &Dialect) -> Vec<String> {
    let Dialect {
        uuid,
        timestamp: ts,
        bytes,
        now,
        ..
    } = d;
    vec![
        // ── attachments ─────────────────────────────────────────────────────
        format!(
            "CREATE TABLE attachments (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
                uploaded_by_user_id {uuid} NOT NULL,
                filename VARCHAR(255) NOT NULL,
                content_type VARCHAR(128),
                size_bytes BIGINT,
                storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
                provider_file_id VARCHAR(128),
                status VARCHAR(16) NOT NULL
                    CONSTRAINT ck_attachments_status CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
                error_code VARCHAR(64),
                attachment_kind VARCHAR(16) NOT NULL
                    CONSTRAINT ck_attachments_kind CHECK (attachment_kind IN ('document', 'image')),
                for_file_search BOOLEAN NOT NULL DEFAULT FALSE,
                for_code_interpreter BOOLEAN NOT NULL DEFAULT FALSE,
                doc_summary TEXT,
                img_thumbnail {bytes},
                img_thumbnail_width INTEGER,
                img_thumbnail_height INTEGER,
                summary_model VARCHAR(1024),
                summary_updated_at {ts},
                cleanup_status VARCHAR(16),
                cleanup_attempts INTEGER NOT NULL DEFAULT 0,
                last_cleanup_error TEXT,
                cleanup_updated_at {ts},
                created_at {ts} NOT NULL,
                updated_at {ts} NOT NULL DEFAULT {now},
                deleted_at {ts},
                secondary_file_id VARCHAR(128),
                secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted'
                    CONSTRAINT ck_attachments_secondary_status CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
                secondary_provider_kind VARCHAR(32)
                    CONSTRAINT ck_attachments_secondary_kind CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind = 'anthropic'),
                CONSTRAINT uq_attachments_id_chat UNIQUE (id, chat_id)
            )"
        ),
        "CREATE INDEX idx_attachments_chat ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL".to_owned(),
        "CREATE INDEX idx_attachments_cleanup ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL".to_owned(),
        // Plain index: the reaper scan binds the status values as parameters
        // and SQLite only uses a partial index when the query repeats its WHERE.
        "CREATE INDEX idx_attachments_stale_upload ON attachments (status, cleanup_status, deleted_at, updated_at)".to_owned(),
        // ── message_attachments ─────────────────────────────────────────────
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
        "CREATE INDEX idx_message_attachments_chat ON message_attachments (tenant_id, chat_id)".to_owned(),
        "CREATE INDEX idx_message_attachments_attachment ON message_attachments (attachment_id, chat_id)".to_owned(),
        // ── thread_summaries ────────────────────────────────────────────────
        format!(
            "CREATE TABLE thread_summaries (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL UNIQUE REFERENCES chats (id) ON DELETE CASCADE,
                summary_text TEXT,
                summarized_up_to_created_at {ts} NOT NULL,
                summarized_up_to_message_id {uuid} NOT NULL,
                token_estimate INTEGER,
                created_at {ts} NOT NULL,
                updated_at {ts} NOT NULL
            )"
        ),
    ]
}

/// `chat_vector_stores`, `quota_usage`, `message_reactions`.
fn usage_statements(d: &Dialect) -> Vec<String> {
    let Dialect {
        uuid,
        timestamp: ts,
        ..
    } = d;
    vec![
        // ── chat_vector_stores ──────────────────────────────────────────────
        format!(
            "CREATE TABLE chat_vector_stores (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                chat_id {uuid} NOT NULL,
                vector_store_id VARCHAR(128),
                provider VARCHAR(128) NOT NULL,
                file_count INTEGER NOT NULL DEFAULT 0
                    CONSTRAINT ck_chat_vector_stores_file_count CHECK (file_count >= 0),
                created_at {ts} NOT NULL,
                CONSTRAINT uq_chat_vector_stores_chat UNIQUE (tenant_id, chat_id)
            )"
        ),
        // ── quota_usage ─────────────────────────────────────────────────────
        format!(
            "CREATE TABLE quota_usage (
                id {uuid} PRIMARY KEY NOT NULL,
                tenant_id {uuid} NOT NULL,
                user_id {uuid} NOT NULL,
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
                updated_at {ts},
                CONSTRAINT uq_quota_usage_bucket UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
            )"
        ),
        // ── message_reactions ───────────────────────────────────────────────
        format!(
            "CREATE TABLE message_reactions (
                id {uuid} PRIMARY KEY NOT NULL,
                message_id {uuid} NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
                user_id {uuid} NOT NULL,
                tenant_id {uuid} NOT NULL,
                reaction VARCHAR(16) NOT NULL
                    CONSTRAINT ck_message_reactions_reaction CHECK (reaction IN ('like', 'dislike')),
                created_at {ts} NOT NULL,
                CONSTRAINT uq_message_reactions_user UNIQUE (message_id, user_id)
            )"
        ),
    ]
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let dialect = dialect(manager.get_database_backend())?;
        let conn = manager.get_connection();
        for statement in statements(&dialect) {
            conn.execute_unprepared(&statement).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        dialect(manager.get_database_backend())?;
        let conn = manager.get_connection();
        for table in TABLES.iter().rev() {
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table}"))
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "m20260301_000001_initial_schema_tests.rs"]
mod tests;
