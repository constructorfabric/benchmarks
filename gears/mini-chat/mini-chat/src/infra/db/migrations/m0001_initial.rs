//! Initial `mini-chat` schema (DESIGN §3.7).
//!
//! Per-backend raw SQL (not the `SeaORM` schema builder) so that `CHECK`
//! constraints, partial unique indexes and composite foreign keys are kept
//! verbatim. Both variants are generated from one template ([`Dialect`]) so
//! the table shapes cannot drift apart; only the column types differ:
//!
//! | logical   | `PostgreSQL`   | `SQLite`                                   |
//! |-----------|----------------|--------------------------------------------|
//! | uuid      | `UUID`         | `TEXT` column holding 16-byte blobs        |
//! | timestamp | `TIMESTAMPTZ`  | `TEXT` (sea-orm `DateTimeUtc`, `+00:00`)   |
//! | date      | `DATE`         | `TEXT` (`YYYY-MM-DD`)                      |
//! | boolean   | `BOOLEAN`      | `INTEGER`                                  |
//! | json      | `JSONB`        | `TEXT`                                     |
//! | bytes     | `BYTEA`        | `BLOB`                                     |
//! | `BIGINT`  | `BIGINT`       | `INTEGER`                                  |
//! | `VARCHAR` | `VARCHAR(n)`   | `TEXT`                                     |
//!
//! Not enforced by the database (ADR-0010): the `chat_turns` cross-column
//! CHECKs and the `attachments.cleanup_status` value CHECK.
//! `MySQL` is not supported; the migration fails fast with a typed error.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseBackend};

const MYSQL_NOT_SUPPORTED: &str = "mini-chat migrations: MySQL is not supported \
    (this migration set targets PostgreSQL/SQLite)";

/// Tables in dependency order (parents first).
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

/// Dialect-specific spellings used by the shared DDL template.
struct Dialect {
    uuid: &'static str,
    ts: &'static str,
    date: &'static str,
    boolean: &'static str,
    json: &'static str,
    bytes: &'static str,
    bigint: &'static str,
    int: &'static str,
    /// `VARCHAR(n)` on `PostgreSQL`; `TEXT` on `SQLite`.
    varchar: fn(u32) -> String,
    false_lit: &'static str,
    empty_json_array: &'static str,
    now: &'static str,
}

const PG: Dialect = Dialect {
    uuid: "UUID",
    ts: "TIMESTAMPTZ",
    date: "DATE",
    boolean: "BOOLEAN",
    json: "JSONB",
    bytes: "BYTEA",
    bigint: "BIGINT",
    int: "INTEGER",
    varchar: |n| format!("VARCHAR({n})"),
    false_lit: "FALSE",
    empty_json_array: "'[]'::jsonb",
    now: "now()",
};

const SQLITE: Dialect = Dialect {
    uuid: "TEXT",
    ts: "TEXT",
    date: "TEXT",
    boolean: "INTEGER",
    json: "TEXT",
    bytes: "BLOB",
    bigint: "INTEGER",
    int: "INTEGER",
    varchar: |_| "TEXT".to_owned(),
    false_lit: "0",
    empty_json_array: "'[]'",
    // Same text shape sqlx writes for `chrono::DateTime<Utc>` (RFC 3339 with
    // `+00:00`; millisecond precision).
    now: "(strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))",
};

/// Full ordered statement list (tables, then indexes) for a dialect.
#[allow(clippy::too_many_lines)]
fn statements(d: &Dialect) -> Vec<String> {
    let Dialect {
        uuid,
        ts,
        date,
        boolean,
        json,
        bytes,
        bigint,
        int,
        false_lit,
        empty_json_array,
        now,
        ..
    } = *d;
    let v16 = (d.varchar)(16);
    let v32 = (d.varchar)(32);
    let v64 = (d.varchar)(64);
    let v128 = (d.varchar)(128);
    let v255 = (d.varchar)(255);
    let v1024 = (d.varchar)(1024);

    let mut out = vec![
        format!(
            r"CREATE TABLE IF NOT EXISTS chats (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    user_id {uuid} NOT NULL,
    model {v1024},
    title {v255},
    is_temporary {boolean} NOT NULL DEFAULT {false_lit},
    created_at {ts} NOT NULL,
    updated_at {ts} NOT NULL,
    deleted_at {ts}
)"
        ),
        format!(
            r"CREATE TABLE IF NOT EXISTS messages (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id {uuid},
    role {v16} NOT NULL,
    content TEXT NOT NULL,
    content_type {v32} NOT NULL DEFAULT 'text',
    token_estimate {int} NOT NULL DEFAULT 0 CHECK (token_estimate >= 0),
    provider_response_id {v128},
    request_kind {v16} NOT NULL DEFAULT 'chat',
    features_used {json} NOT NULL DEFAULT {empty_json_array},
    input_tokens {bigint} NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens {bigint} NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cache_read_input_tokens {bigint} NOT NULL DEFAULT 0 CHECK (cache_read_input_tokens >= 0),
    cache_write_input_tokens {bigint} NOT NULL DEFAULT 0 CHECK (cache_write_input_tokens >= 0),
    reasoning_tokens {bigint} NOT NULL DEFAULT 0 CHECK (reasoning_tokens >= 0),
    model {v1024},
    is_compressed {boolean} NOT NULL DEFAULT {false_lit},
    created_at {ts} NOT NULL,
    deleted_at {ts},
    CONSTRAINT uq_messages_id_chat UNIQUE (id, chat_id)
)"
        ),
        format!(
            r"CREATE TABLE IF NOT EXISTS chat_turns (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    request_id {uuid} NOT NULL,
    requester_type {v16} NOT NULL CHECK (requester_type IN ('user', 'system')),
    requester_user_id {uuid},
    state {v16} NOT NULL CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
    provider_name {v128},
    provider_response_id {v128},
    assistant_message_id {uuid},
    error_code {v64},
    reserve_tokens {bigint},
    max_output_tokens_applied {int},
    reserved_credits_micro {bigint},
    policy_version_applied {bigint},
    effective_model {v1024},
    minimal_generation_floor_applied {int},
    error_detail TEXT,
    deleted_at {ts},
    replaced_by_request_id {uuid},
    started_at {ts} NOT NULL,
    last_progress_at {ts},
    web_search_enabled {boolean} NOT NULL DEFAULT {false_lit},
    web_search_completed_count {int} NOT NULL DEFAULT 0 CHECK (web_search_completed_count >= 0),
    code_interpreter_completed_count {int} NOT NULL DEFAULT 0 CHECK (code_interpreter_completed_count >= 0),
    file_search_completed_count {int} NOT NULL DEFAULT 0 CHECK (file_search_completed_count >= 0),
    completed_at {ts},
    updated_at {ts},
    CONSTRAINT uq_chat_turns_chat_request UNIQUE (chat_id, request_id)
)"
        ),
        format!(
            r"CREATE TABLE IF NOT EXISTS attachments (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    uploaded_by_user_id {uuid} NOT NULL,
    filename {v255} NOT NULL,
    content_type {v128},
    size_bytes {bigint} CHECK (size_bytes >= 0),
    storage_backend {v32} NOT NULL DEFAULT 'azure',
    provider_file_id {v128},
    status {v16} NOT NULL CHECK (status IN ('pending', 'uploaded', 'ready', 'failed')),
    error_code {v64},
    attachment_kind {v16} NOT NULL CHECK (attachment_kind IN ('document', 'image')),
    for_file_search {boolean} NOT NULL DEFAULT {false_lit},
    for_code_interpreter {boolean} NOT NULL DEFAULT {false_lit},
    doc_summary TEXT,
    img_thumbnail {bytes},
    img_thumbnail_width {int},
    img_thumbnail_height {int},
    summary_model {v1024},
    summary_updated_at {ts},
    cleanup_status {v16},
    cleanup_attempts {int} NOT NULL DEFAULT 0 CHECK (cleanup_attempts >= 0),
    last_cleanup_error TEXT,
    cleanup_updated_at {ts},
    created_at {ts} NOT NULL,
    updated_at {ts} NOT NULL DEFAULT {now},
    deleted_at {ts},
    secondary_file_id {v128},
    secondary_status {v16} NOT NULL DEFAULT 'not_attempted'
        CHECK (secondary_status IN ('not_attempted', 'pending', 'uploaded', 'failed')),
    secondary_provider_kind {v32}
        CHECK (secondary_provider_kind IS NULL OR secondary_provider_kind IN ('anthropic')),
    CONSTRAINT uq_attachments_id_chat UNIQUE (id, chat_id)
)"
        ),
        format!(
            r"CREATE TABLE IF NOT EXISTS message_attachments (
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
        format!(
            r"CREATE TABLE IF NOT EXISTS thread_summaries (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    summary_text TEXT,
    summarized_up_to_created_at {ts} NOT NULL,
    summarized_up_to_message_id {uuid} NOT NULL,
    token_estimate {int} CHECK (token_estimate >= 0),
    created_at {ts} NOT NULL,
    updated_at {ts} NOT NULL,
    CONSTRAINT uq_thread_summaries_chat UNIQUE (chat_id)
)"
        ),
        // No FK to `chats`: a soft-deleted chat keeps its row until the provider
        // store is deleted (cleanup invariant, DESIGN §3.7).
        format!(
            r"CREATE TABLE IF NOT EXISTS chat_vector_stores (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    chat_id {uuid} NOT NULL,
    vector_store_id {v128},
    provider {v128} NOT NULL,
    file_count {int} NOT NULL DEFAULT 0 CHECK (file_count >= 0),
    created_at {ts} NOT NULL,
    CONSTRAINT uq_chat_vector_stores_tenant_chat UNIQUE (tenant_id, chat_id)
)"
        ),
        format!(
            r"CREATE TABLE IF NOT EXISTS quota_usage (
    id {uuid} PRIMARY KEY NOT NULL,
    tenant_id {uuid} NOT NULL,
    user_id {uuid} NOT NULL,
    period_type {v16} NOT NULL,
    period_start {date} NOT NULL,
    bucket {v32} NOT NULL,
    spent_credits_micro {bigint} NOT NULL DEFAULT 0 CHECK (spent_credits_micro >= 0),
    reserved_credits_micro {bigint} NOT NULL DEFAULT 0 CHECK (reserved_credits_micro >= 0),
    calls {int} NOT NULL DEFAULT 0 CHECK (calls >= 0),
    input_tokens {bigint} NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens {bigint} NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    file_search_calls {int} NOT NULL DEFAULT 0 CHECK (file_search_calls >= 0),
    web_search_calls {int} NOT NULL DEFAULT 0 CHECK (web_search_calls >= 0),
    code_interpreter_calls {int} NOT NULL DEFAULT 0 CHECK (code_interpreter_calls >= 0),
    rag_retrieval_calls {int} NOT NULL DEFAULT 0 CHECK (rag_retrieval_calls >= 0),
    image_inputs {int} NOT NULL DEFAULT 0 CHECK (image_inputs >= 0),
    image_upload_bytes {bigint} NOT NULL DEFAULT 0 CHECK (image_upload_bytes >= 0),
    updated_at {ts},
    CONSTRAINT uq_quota_usage_bucket UNIQUE (tenant_id, user_id, period_type, period_start, bucket)
)"
        ),
        format!(
            r"CREATE TABLE IF NOT EXISTS message_reactions (
    id {uuid} PRIMARY KEY NOT NULL,
    message_id {uuid} NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id {uuid} NOT NULL,
    tenant_id {uuid} NOT NULL,
    reaction {v16} NOT NULL CHECK (reaction IN ('like', 'dislike')),
    created_at {ts} NOT NULL,
    CONSTRAINT uq_message_reactions_message_user UNIQUE (message_id, user_id)
)"
        ),
    ];

    // Indexes (identical on both engines; partial indexes are supported by both).
    out.extend(
        [
            "CREATE INDEX IF NOT EXISTS idx_chats_tenant_user_updated ON chats (tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL",
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_chat_request_role ON messages (chat_id, request_id, role) WHERE request_id IS NOT NULL AND deleted_at IS NULL",
            "CREATE INDEX IF NOT EXISTS idx_messages_chat_created ON messages (chat_id, created_at) WHERE deleted_at IS NULL",
            "CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started ON chat_turns (chat_id, started_at DESC) WHERE deleted_at IS NULL",
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_one_running_per_chat ON chat_turns (chat_id) WHERE state = 'running' AND deleted_at IS NULL",
            "CREATE INDEX IF NOT EXISTS idx_chat_turns_orphan_scan ON chat_turns (last_progress_at) WHERE state = 'running' AND deleted_at IS NULL",
            "CREATE INDEX IF NOT EXISTS idx_attachments_tenant_chat ON attachments (tenant_id, chat_id) WHERE deleted_at IS NULL",
            "CREATE INDEX IF NOT EXISTS idx_attachments_cleanup ON attachments (cleanup_status) WHERE cleanup_status IS NOT NULL AND deleted_at IS NULL",
            // Plain (not partial): the reaper binds the status values as parameters.
            "CREATE INDEX IF NOT EXISTS idx_attachments_stale_upload ON attachments (status, cleanup_status, deleted_at, updated_at)",
            "CREATE INDEX IF NOT EXISTS idx_message_attachments_tenant_chat ON message_attachments (tenant_id, chat_id)",
            "CREATE INDEX IF NOT EXISTS idx_message_attachments_attachment_chat ON message_attachments (attachment_id, chat_id)",
        ]
        .map(str::to_owned),
    );
    out
}

/// `PostgreSQL` statements (tables then indexes).
#[must_use]
pub fn pg_statements() -> Vec<String> {
    statements(&PG)
}

/// `SQLite` statements (tables then indexes).
#[must_use]
pub fn sqlite_statements() -> Vec<String> {
    statements(&SQLITE)
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let stmts = match manager.get_database_backend() {
            DatabaseBackend::Postgres => pg_statements(),
            DatabaseBackend::Sqlite => sqlite_statements(),
            _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        };
        let conn = manager.get_connection();
        for sql in stmts {
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
