//! Initial mini-chat schema (DESIGN section 3.7).
//!
//! Creates `chats`, `messages`, `chat_turns`, `attachments`,
//! `message_attachments`, `thread_summaries`, `chat_vector_stores`,
//! `quota_usage` and `message_reactions`. The MCP tables are intentionally not
//! created (ADR-0006).
//!
//! The same DDL template serves both engines; only the column types differ
//! (see [`Dialect`]). `SQLite` declares UUID columns `TEXT` although sqlx writes
//! them as 16-byte BLOBs. Only simple value CHECKs and non-negative counter
//! CHECKs exist: the cross-column `chat_turns` invariants and the
//! `attachments.cleanup_status` value set are kept by the repositories
//! (ADR-0010). Every constraint lives in the `CREATE TABLE` statement because
//! `SQLite` cannot add constraints later. One statement per `execute_unprepared`.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend};

const MYSQL_NOT_SUPPORTED: &str = "mini-chat migrations: MySQL is not supported \
    (this migration set targets PostgreSQL/SQLite)";

/// Column types per backend.
struct Dialect {
    uuid: &'static str,
    ts: &'static str,
    date: &'static str,
    boolean: &'static str,
    json: &'static str,
    bytes: &'static str,
    /// Default expression for `updated_at` columns written outside the gear.
    ts_now: &'static str,
}

const POSTGRES: Dialect = Dialect {
    uuid: "UUID",
    ts: "TIMESTAMPTZ",
    date: "DATE",
    boolean: "BOOLEAN",
    json: "JSONB",
    bytes: "BYTEA",
    ts_now: "now()",
};

const SQLITE: Dialect = Dialect {
    uuid: "TEXT",
    ts: "TEXT",
    date: "TEXT",
    boolean: "BOOLEAN",
    json: "TEXT",
    bytes: "BLOB",
    // Same RFC 3339 text shape that sqlx and `db_ts` write (UTC, nine fraction digits).
    ts_now: "(strftime('%Y-%m-%dT%H:%M:%S', 'now') || '.000000001Z')",
};

/// Tables in dependency order; dropped in reverse.
const TABLES: &[&str] = &[
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

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &'static str {
        "m20261004_000001_mini_chat_initial"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let dialect = match manager.get_database_backend() {
            DatabaseBackend::Postgres => &POSTGRES,
            DatabaseBackend::Sqlite => &SQLITE,
            _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        };
        let conn = manager.get_connection();
        for sql in up_statements(dialect) {
            conn.execute_unprepared(&sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !matches!(
            manager.get_database_backend(),
            DatabaseBackend::Postgres | DatabaseBackend::Sqlite
        ) {
            return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned()));
        }
        let conn = manager.get_connection();
        for table in TABLES.iter().rev() {
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table}"))
                .await?;
        }
        Ok(())
    }
}

/// All DDL statements, tables first (dependency order), then indexes.
fn up_statements(d: &Dialect) -> Vec<String> {
    let mut out = chat_tables(d);
    out.extend(attachment_tables(d));
    out.extend(other_tables(d));
    out.extend(INDEXES.iter().map(|s| (*s).to_owned()));
    out
}

/// `chats`, `messages`, `chat_turns`.
fn chat_tables(d: &Dialect) -> Vec<String> {
    let Dialect {
        uuid,
        ts,
        boolean,
        json,
        ..
    } = d;
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS chats (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    user_id {uuid} NOT NULL,
    model VARCHAR(1024) NOT NULL,
    title VARCHAR(255) NULL,
    is_temporary {boolean} NOT NULL DEFAULT FALSE,
    created_at {ts} NOT NULL,
    updated_at {ts} NOT NULL,
    deleted_at {ts} NULL
)"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS messages (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    request_id {uuid} NULL,
    role VARCHAR(16) NOT NULL,
    content TEXT NOT NULL,
    content_type VARCHAR(32) NOT NULL DEFAULT 'text',
    token_estimate INTEGER NOT NULL DEFAULT 0,
    provider_response_id VARCHAR(128) NULL,
    request_kind VARCHAR(16) NOT NULL DEFAULT 'chat',
    features_used {json} NOT NULL DEFAULT '[]',
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cache_read_input_tokens BIGINT NOT NULL DEFAULT 0,
    cache_write_input_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    model VARCHAR(1024) NULL,
    is_compressed {boolean} NOT NULL DEFAULT FALSE,
    created_at {ts} NOT NULL,
    deleted_at {ts} NULL,
    CONSTRAINT ck_messages_role CHECK (role IN ('user', 'assistant', 'system')),
    CONSTRAINT ck_messages_counters CHECK (
        token_estimate >= 0 AND input_tokens >= 0 AND output_tokens >= 0
        AND cache_read_input_tokens >= 0 AND cache_write_input_tokens >= 0
        AND reasoning_tokens >= 0
    ),
    CONSTRAINT fk_messages_chat FOREIGN KEY (chat_id) REFERENCES chats (id) ON DELETE CASCADE,
    CONSTRAINT uq_messages_id_chat UNIQUE (id, chat_id)
)"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS chat_turns (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    request_id {uuid} NOT NULL,
    requester_type VARCHAR(16) NOT NULL,
    requester_user_id {uuid} NULL,
    state VARCHAR(16) NOT NULL,
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
    web_search_enabled {boolean} NOT NULL DEFAULT FALSE,
    web_search_completed_count INTEGER NOT NULL DEFAULT 0,
    code_interpreter_completed_count INTEGER NOT NULL DEFAULT 0,
    file_search_completed_count INTEGER NOT NULL DEFAULT 0,
    completed_at {ts} NULL,
    updated_at {ts} NOT NULL,
    CONSTRAINT ck_chat_turns_requester_type CHECK (requester_type IN ('user', 'system')),
    CONSTRAINT ck_chat_turns_state CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
    CONSTRAINT ck_chat_turns_counters CHECK (
        web_search_completed_count >= 0 AND code_interpreter_completed_count >= 0
        AND file_search_completed_count >= 0
    ),
    CONSTRAINT fk_chat_turns_chat FOREIGN KEY (chat_id) REFERENCES chats (id) ON DELETE CASCADE,
    CONSTRAINT uq_chat_turns_chat_request UNIQUE (chat_id, request_id)
)"
        ),
    ]
}

/// `attachments`, `message_attachments`.
fn attachment_tables(d: &Dialect) -> Vec<String> {
    let Dialect {
        uuid,
        ts,
        boolean,
        bytes,
        ts_now,
        ..
    } = d;
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS attachments (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    uploaded_by_user_id {uuid} NOT NULL,
    filename VARCHAR(255) NOT NULL,
    content_type VARCHAR(128) NOT NULL,
    size_bytes BIGINT NOT NULL DEFAULT 0,
    storage_backend VARCHAR(32) NOT NULL DEFAULT 'azure',
    provider_file_id VARCHAR(128) NULL,
    status VARCHAR(16) NOT NULL,
    error_code VARCHAR(64) NULL,
    attachment_kind VARCHAR(16) NOT NULL,
    for_file_search {boolean} NOT NULL DEFAULT FALSE,
    for_code_interpreter {boolean} NOT NULL DEFAULT FALSE,
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
    updated_at {ts} NOT NULL DEFAULT {ts_now},
    deleted_at {ts} NULL,
    secondary_file_id VARCHAR(128) NULL,
    secondary_status VARCHAR(16) NOT NULL DEFAULT 'not_attempted',
    secondary_provider_kind VARCHAR(32) NULL,
    CONSTRAINT ck_attachments_kind CHECK (attachment_kind IN ('document', 'image')),
    CONSTRAINT ck_attachments_status CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    CONSTRAINT ck_attachments_secondary_status CHECK (
        secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')
    ),
    CONSTRAINT ck_attachments_secondary_provider_kind CHECK (
        secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic')
    ),
    CONSTRAINT ck_attachments_counters CHECK (size_bytes >= 0 AND cleanup_attempts >= 0),
    CONSTRAINT fk_attachments_chat FOREIGN KEY (chat_id) REFERENCES chats (id) ON DELETE CASCADE,
    CONSTRAINT uq_attachments_id_chat UNIQUE (id, chat_id)
)"
        ),
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
    ]
}

/// `thread_summaries`, `chat_vector_stores`, `quota_usage`, `message_reactions`.
fn other_tables(d: &Dialect) -> Vec<String> {
    let Dialect {
        uuid,
        ts,
        date,
        ts_now,
        ..
    } = d;
    vec![
        format!(
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
    CONSTRAINT ck_thread_summaries_token_estimate CHECK (token_estimate >= 0),
    CONSTRAINT fk_thread_summaries_chat FOREIGN KEY (chat_id) REFERENCES chats (id) ON DELETE CASCADE,
    CONSTRAINT uq_thread_summaries_chat UNIQUE (chat_id)
)"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    vector_store_id VARCHAR(128) NULL,
    provider VARCHAR(128) NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0,
    created_at {ts} NOT NULL,
    CONSTRAINT ck_chat_vector_stores_file_count CHECK (file_count >= 0),
    CONSTRAINT fk_chat_vector_stores_chat FOREIGN KEY (chat_id) REFERENCES chats (id) ON DELETE CASCADE,
    CONSTRAINT uq_chat_vector_stores_tenant_chat UNIQUE (tenant_id, chat_id)
)"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS quota_usage (
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
    updated_at {ts} NOT NULL DEFAULT {ts_now},
    CONSTRAINT ck_quota_usage_counters CHECK (
        spent_credits_micro >= 0 AND reserved_credits_micro >= 0 AND calls >= 0
        AND input_tokens >= 0 AND output_tokens >= 0 AND file_search_calls >= 0
        AND web_search_calls >= 0 AND code_interpreter_calls >= 0
        AND rag_retrieval_calls >= 0 AND image_inputs >= 0 AND image_upload_bytes >= 0
    ),
    CONSTRAINT uq_quota_usage_bucket UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
)"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS message_reactions (
    id {uuid} PRIMARY KEY NOT NULL,
    message_id {uuid} NOT NULL,
    user_id {uuid} NOT NULL,
    tenant_id {uuid} NOT NULL,
    reaction VARCHAR(16) NOT NULL,
    created_at {ts} NOT NULL,
    CONSTRAINT ck_message_reactions_reaction CHECK (reaction IN ('like', 'dislike')),
    CONSTRAINT fk_message_reactions_message FOREIGN KEY (message_id) REFERENCES messages (id) ON DELETE CASCADE,
    CONSTRAINT uq_message_reactions_message_user UNIQUE (message_id, user_id)
)"
        ),
    ]
}

/// Indexes: partial-index and `DESC` syntax is identical on `PostgreSQL` and `SQLite`.
const INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_chats_tenant_user_updated ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_messages_chat_created ON messages (chat_id, created_at) WHERE deleted_at IS NULL",
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role ON messages (chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL",
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_running ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_chat_turns_orphan_scan ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_attachments_cleanup_status ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload ON attachments (status, cleanup_status, deleted_at, updated_at)",
    "CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat ON message_attachments (tenant_id, chat_id)",
    "CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment_chat ON message_attachments (attachment_id, chat_id)",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_render_for_both_backends() {
        for (name, d) in [("postgres", &POSTGRES), ("sqlite", &SQLITE)] {
            let stmts = up_statements(d);
            let tables = stmts
                .iter()
                .filter(|s| s.starts_with("CREATE TABLE"))
                .count();
            assert_eq!(tables, TABLES.len(), "{name}");
            for s in &stmts {
                assert!(!s.contains('{') && !s.contains('}'), "{name}: {s}");
            }
            for t in TABLES {
                assert!(
                    stmts
                        .iter()
                        .any(|s| s.starts_with(&format!("CREATE TABLE IF NOT EXISTS {t} ("))),
                    "{name}: {t}"
                );
            }
        }
        let pg = up_statements(&POSTGRES).join("\n");
        assert!(pg.contains("id UUID PRIMARY KEY") && pg.contains("TIMESTAMPTZ"));
        assert!(pg.contains("JSONB") && pg.contains("BYTEA") && pg.contains("DATE"));
        let sqlite = up_statements(&SQLITE).join("\n");
        assert!(sqlite.contains("id TEXT PRIMARY KEY") && !sqlite.contains("length("));
    }
}
