#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, unused_imports)]

//! Helpers of the slice S4 turn-mutation tests (retry, edit and delete of the latest turn,
//! DESIGN sections 3.6 "Retry / edit variant" and 3.9 "Turn Mutation Rules").
//!
//! They use only the harness API of S1-S3 plus the routes of the three mutation endpoints:
//! `POST /v1/chats/{id}/turns/{request_id}/retry`, `PATCH ...` (edit) and `DELETE ...`.
//!
//! A test file that needs them declares `mod common; mod live; mod mutation;`.

use http::{Method, StatusCode};
use mini_chat::test_support::{SseEvent, StreamingResponse, TestEnv, TestResponse, TestUser};
use sea_orm::{ConnectionTrait, DbBackend, QueryResult, Statement};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::common::{
    AUDIT_QUEUE, AttachmentSpec, CompletedTurn, SQL_TS, Seed, TurnRow, assert_aborted,
    assert_failed_precondition, blob, complete_turn, enqueue_text, env_two_models, exec_sql,
    expect_stream, insert_attachment_row, limits, merge, new_chat, policy, scalar_i64, scalar_text,
    seed_quota, send_with_id, terminal, turn_path, turn_rows, two_model_catalog, uuid_hex,
};

/// The content an edit sends.
pub const EDITED: &str = "EDITED-CONTENT";

/// The answer of the turn a retry or an edit starts.
pub const NEW_ANSWER: &str = "NEW-ANSWER";

// ---------------------------------------------------------------------------
// The three operations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Retry,
    Edit,
    Delete,
}

impl Op {
    /// Retry and edit start a new turn; delete does not.
    #[must_use]
    pub fn replaces(self) -> bool {
        self != Op::Delete
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Op::Retry => "retry",
            Op::Edit => "edit",
            Op::Delete => "delete",
        }
    }

    /// The `event_type` of the mutation audit event.
    #[must_use]
    pub fn audit_type(self) -> &'static str {
        match self {
            Op::Retry => "turn_retry",
            Op::Edit => "turn_edit",
            Op::Delete => "turn_delete",
        }
    }
}

/// Generates one `#[tokio::test]` per operation (`retry`, `edit`, `delete`) that runs `$check`.
#[allow(unused_macros)]
macro_rules! per_op {
    ($group:ident, $check:ident) => {
        mod $group {
            use super::*;

            #[tokio::test]
            async fn retry() {
                $check(Op::Retry).await;
            }

            #[tokio::test]
            async fn edit() {
                $check(Op::Edit).await;
            }

            #[tokio::test]
            async fn delete() {
                $check(Op::Delete).await;
            }
        }
    };
}

/// Like `per_op!` for the operations that start a new turn (`retry` and `edit`).
#[allow(unused_macros)]
macro_rules! per_replacing_op {
    ($group:ident, $check:ident) => {
        mod $group {
            use super::*;

            #[tokio::test]
            async fn retry() {
                $check(Op::Retry).await;
            }

            #[tokio::test]
            async fn edit() {
                $check(Op::Edit).await;
            }
        }
    };
}

pub(crate) use per_op;
pub(crate) use per_replacing_op;

#[must_use]
pub fn retry_path(chat: &str, request_id: Uuid) -> String {
    format!("{}/retry", turn_path(chat, &request_id.to_string()))
}

pub async fn retry(env: &TestEnv, user: &TestUser, chat: &str, request_id: Uuid) -> TestResponse {
    env.request(user, Method::POST, &retry_path(chat, request_id), None)
        .await
}

pub async fn edit(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    request_id: Uuid,
    content: &str,
) -> TestResponse {
    env.request(
        user,
        Method::PATCH,
        &turn_path(chat, &request_id.to_string()),
        Some(json!({ "content": content })),
    )
    .await
}

pub async fn delete_turn(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    request_id: Uuid,
) -> TestResponse {
    env.request(
        user,
        Method::DELETE,
        &turn_path(chat, &request_id.to_string()),
        None,
    )
    .await
}

/// Deletes the turn and asserts `204`.
pub async fn delete_ok(env: &TestEnv, user: &TestUser, chat: &str, request_id: Uuid) {
    let resp = delete_turn(env, user, chat, request_id).await;
    assert_eq!(
        resp.status,
        StatusCode::NO_CONTENT,
        "{}",
        common_body(&resp)
    );
}

/// Runs `op` against the turn (an edit sends [`EDITED`]).
pub async fn mutate(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    request_id: Uuid,
    op: Op,
) -> TestResponse {
    match op {
        Op::Retry => retry(env, user, chat, request_id).await,
        Op::Edit => edit(env, user, chat, request_id, EDITED).await,
        Op::Delete => delete_turn(env, user, chat, request_id).await,
    }
}

/// Queues the provider answer of the turn a retry or an edit starts (nothing for a delete).
pub fn prepare(env: &TestEnv, op: Op) {
    if op.replaces() {
        enqueue_text(env, &[NEW_ANSWER], 10, 5);
    }
}

/// Opens a retry or an edit as a live stream (returns once the headers are there).
pub async fn open_mutation(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    request_id: Uuid,
    op: Op,
) -> StreamingResponse {
    match op {
        Op::Retry => {
            env.request_streaming(user, Method::POST, &retry_path(chat, request_id), None)
                .await
        }
        Op::Edit => {
            env.request_streaming(
                user,
                Method::PATCH,
                &turn_path(chat, &request_id.to_string()),
                Some(json!({ "content": EDITED })),
            )
            .await
        }
        Op::Delete => panic!("a delete has no event stream"),
    }
}

/// Asserts that `op` succeeded: a completed event stream for retry and edit, `204` without a body
/// for delete. Returns the events of a stream.
pub fn expect_mutated(resp: &TestResponse, op: Op) -> Vec<SseEvent> {
    if op.replaces() {
        let events = expect_stream(resp);
        assert_eq!(
            terminal(&events).event,
            "done",
            "{}: expected a completed stream: {events:?}",
            op.name()
        );
        events
    } else {
        assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", common_body(resp));
        assert!(resp.body.is_empty(), "204 carries no body");
        Vec::new()
    }
}

fn common_body(resp: &TestResponse) -> String {
    crate::common::body_text(resp)
}

pub fn assert_not_latest(resp: &TestResponse) -> Value {
    assert_aborted(resp, "NOT_LATEST_TURN")
}

pub fn assert_target_running(resp: &TestResponse) -> Value {
    assert_failed_precondition(resp, "turn_state", "STATE")
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A two-model environment, a user and a chat that already holds completed turns.
pub struct Conversation {
    pub env: TestEnv,
    pub user: TestUser,
    pub chat: String,
    pub turns: Vec<CompletedTurn>,
}

impl Conversation {
    #[must_use]
    pub fn first(&self) -> &CompletedTurn {
        self.turns.first().expect("the conversation has turns")
    }

    #[must_use]
    pub fn last(&self) -> &CompletedTurn {
        self.turns.last().expect("the conversation has turns")
    }
}

/// Completes one turn per `(question, answer)` pair in a fresh chat of the two-model environment.
pub async fn conversation(pairs: &[(&str, &str)]) -> Conversation {
    conversation_in(env_two_models().await, pairs).await
}

/// Like [`conversation`] in a prepared environment.
pub async fn conversation_in(env: TestEnv, pairs: &[(&str, &str)]) -> Conversation {
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let mut turns = Vec::new();
    for (question, answer) in pairs {
        turns.push(complete_turn(&env, &user, &chat, question, answer).await);
    }
    Conversation {
        env,
        user,
        chat,
        turns,
    }
}

/// Limits under which a single reserve fits but `exhaust_quota` leaves no room.
pub async fn env_with_daily_cap() -> TestEnv {
    crate::common::env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "default_standard_limits": limits(1_000_000, 1_000_000_000) }),
    ))
    .await
}

/// A two-model environment in which the premium (default) model has a 4000 token window and a
/// 1000 token output cap: input limit 3000, token budget 2900 (DESIGN 3.4).
pub async fn env_small_window() -> TestEnv {
    crate::common::env_with_policy(policy(small_window_catalog())).await
}

#[must_use]
pub fn small_window_catalog() -> Vec<Value> {
    vec![
        merge(
            crate::common::catalog_entry(crate::common::PREMIUM_MODEL, "Premium", true, true),
            json!({ "context_window": 4000, "max_output_tokens": 1000 }),
        ),
        crate::common::catalog_entry(crate::common::STANDARD_MODEL, "Standard", true, false),
    ]
}

/// Spends the whole daily overall cap of `user` (every tier is exhausted afterwards).
pub async fn exhaust_quota(env: &TestEnv, user: &TestUser) {
    seed_quota(env, user, "daily", "total", Seed::spent(1_000_000)).await;
}

// ---------------------------------------------------------------------------
// Direct database access
// ---------------------------------------------------------------------------

async fn rows(env: &TestEnv, sql: String) -> Vec<QueryResult> {
    env.db()
        .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
        .expect("row query must succeed")
}

fn opt_uuid(row: &QueryResult, column: &str) -> Option<Uuid> {
    let hex: Option<String> = row.try_get("", column).unwrap();
    hex.filter(|hex| !hex.is_empty())
        .map(|hex| Uuid::parse_str(&hex).expect("a 16-byte UUID blob"))
}

fn chat_uuid(chat: &str) -> Uuid {
    Uuid::parse_str(chat).expect("chat id must be a UUID")
}

/// The mutation columns of a `chat_turns` row.
#[derive(Debug, Clone)]
pub struct TurnDetail {
    pub request_id: Uuid,
    pub state: String,
    pub deleted: bool,
    pub replaced_by: Option<Uuid>,
    pub web_search_enabled: bool,
    pub requester_user_id: Option<Uuid>,
    pub error_code: Option<String>,
}

pub async fn turn_detail(env: &TestEnv, chat: &str, request_id: Uuid) -> TurnDetail {
    let sql = format!(
        "SELECT state, error_code, CASE WHEN deleted_at IS NULL THEN 0 ELSE 1 END AS is_deleted, \
         hex(replaced_by_request_id) AS replaced_hex, web_search_enabled, \
         hex(requester_user_id) AS user_hex FROM chat_turns \
         WHERE hex(chat_id) = '{}' AND hex(request_id) = '{}'",
        uuid_hex(chat_uuid(chat)),
        uuid_hex(request_id)
    );
    let found = rows(env, sql).await;
    let row = found
        .first()
        .unwrap_or_else(|| panic!("no chat_turns row for {request_id}"));
    TurnDetail {
        request_id,
        state: row.try_get("", "state").unwrap(),
        deleted: row.try_get::<i64>("", "is_deleted").unwrap() != 0,
        replaced_by: opt_uuid(row, "replaced_hex"),
        web_search_enabled: row.try_get::<i64>("", "web_search_enabled").unwrap() != 0,
        requester_user_id: opt_uuid(row, "user_hex"),
        error_code: row.try_get("", "error_code").unwrap(),
    }
}

/// Non-deleted turns of the chat, oldest first.
pub async fn live_turns(env: &TestEnv, chat: &str) -> Vec<TurnRow> {
    turn_rows(env, chat)
        .await
        .into_iter()
        .filter(|turn| !turn.deleted)
        .collect()
}

/// Non-deleted `running` turns of the chat.
pub async fn running_turns(env: &TestEnv, chat: &str) -> Vec<TurnRow> {
    live_turns(env, chat)
        .await
        .into_iter()
        .filter(|turn| turn.state == "running")
        .collect()
}

/// The request id of the newest turn row (soft-deleted ones included).
pub async fn newest_request_id(env: &TestEnv, chat: &str) -> Uuid {
    turn_rows(env, chat)
        .await
        .last()
        .expect("the chat has turns")
        .request_id
}

/// A `messages` row with the columns the mutation tests read.
#[derive(Debug, Clone)]
pub struct FullMessage {
    pub id: Uuid,
    pub request_id: Option<Uuid>,
    pub role: String,
    pub content: String,
    pub deleted: bool,
    pub compressed: bool,
}

/// Every message of the chat (soft-deleted ones included) in `(created_at, insertion)` order.
pub async fn all_messages(env: &TestEnv, chat: &str) -> Vec<FullMessage> {
    let sql = format!(
        "SELECT hex(id) AS id_hex, hex(request_id) AS request_hex, role, content, \
         CASE WHEN deleted_at IS NULL THEN 0 ELSE 1 END AS is_deleted, \
         CASE WHEN is_compressed THEN 1 ELSE 0 END AS compressed \
         FROM messages WHERE hex(chat_id) = '{}' ORDER BY created_at, rowid",
        uuid_hex(chat_uuid(chat))
    );
    rows(env, sql)
        .await
        .iter()
        .map(|row| FullMessage {
            id: opt_uuid(row, "id_hex").unwrap(),
            request_id: opt_uuid(row, "request_hex"),
            role: row.try_get("", "role").unwrap(),
            content: row.try_get("", "content").unwrap(),
            deleted: row.try_get::<i64>("", "is_deleted").unwrap() != 0,
            compressed: row.try_get::<i64>("", "compressed").unwrap() != 0,
        })
        .collect()
}

/// The `role` message of the turn (soft-deleted ones included).
pub async fn message_of(env: &TestEnv, chat: &str, request_id: Uuid, role: &str) -> FullMessage {
    all_messages(env, chat)
        .await
        .into_iter()
        .find(|message| message.request_id == Some(request_id) && message.role == role)
        .unwrap_or_else(|| panic!("no `{role}` message for the turn {request_id}"))
}

/// `(user message, assistant message)` of the turn.
pub async fn messages_of(
    env: &TestEnv,
    chat: &str,
    request_id: Uuid,
) -> (FullMessage, FullMessage) {
    (
        message_of(env, chat, request_id, "user").await,
        message_of(env, chat, request_id, "assistant").await,
    )
}

/// Makes `chats.updated_at` old, so a later bump is visible.
pub async fn age_chat(env: &TestEnv, chat: &str) {
    exec_sql(
        env,
        &format!(
            "UPDATE chats SET updated_at = '2000-01-01T00:00:00Z' WHERE hex(id) = '{}'",
            uuid_hex(chat_uuid(chat))
        ),
    )
    .await;
}

pub async fn chat_updated_at(env: &TestEnv, chat: &str) -> OffsetDateTime {
    let text = scalar_text(
        env,
        &format!(
            "SELECT CAST(updated_at AS TEXT) AS t FROM chats WHERE hex(id) = '{}'",
            uuid_hex(chat_uuid(chat))
        ),
    )
    .await;
    OffsetDateTime::parse(&text, &Rfc3339).unwrap_or_else(|err| panic!("`{text}`: {err}"))
}

/// A snapshot of everything a rejected mutation must leave alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatSnapshot {
    pub turns: Vec<(Uuid, String, bool, Option<Uuid>)>,
    pub messages: Vec<(Uuid, bool, bool)>,
    pub links: i64,
    pub summaries: i64,
    pub mutation_audits: usize,
    pub reserved: i64,
}

pub async fn snapshot(env: &TestEnv, chat: &str) -> ChatSnapshot {
    let mut turns = Vec::new();
    for turn in turn_rows(env, chat).await {
        let detail = turn_detail(env, chat, turn.request_id).await;
        turns.push((
            turn.request_id,
            detail.state,
            detail.deleted,
            detail.replaced_by,
        ));
    }
    let chat_hex = uuid_hex(chat_uuid(chat));
    ChatSnapshot {
        turns,
        messages: all_messages(env, chat)
            .await
            .into_iter()
            .map(|message| (message.id, message.deleted, message.compressed))
            .collect(),
        links: scalar_i64(
            env,
            &format!(
                "SELECT COUNT(*) AS n FROM message_attachments WHERE hex(chat_id) = '{chat_hex}'"
            ),
        )
        .await,
        summaries: scalar_i64(
            env,
            &format!(
                "SELECT COUNT(*) AS n FROM thread_summaries WHERE hex(chat_id) = '{chat_hex}'"
            ),
        )
        .await,
        mutation_audits: mutation_audits(env).await.len(),
        reserved: crate::common::total_reserved(env).await,
    }
}

/// Asserts that a rejected mutation left the chat, the provider, the quota reserve and the
/// mutation audit trail untouched (`before` is a [`snapshot`] taken before the call).
pub async fn assert_untouched(
    env: &TestEnv,
    chat: &str,
    before: &ChatSnapshot,
    provider_calls: usize,
) {
    assert_eq!(
        &snapshot(env, chat).await,
        before,
        "a rejected mutation changes nothing (turns, messages, links, summary, audit trail, reserve)"
    );
    assert_eq!(
        env.provider().chat_requests().len(),
        provider_calls,
        "a rejected mutation must not reach the provider"
    );
}

// ---------------------------------------------------------------------------
// Audit events
// ---------------------------------------------------------------------------

/// The `turn_retry` / `turn_edit` / `turn_delete` events enqueued so far, in enqueue order.
pub async fn mutation_audits(env: &TestEnv) -> Vec<Value> {
    env.outbox_payloads(AUDIT_QUEUE)
        .await
        .into_iter()
        .filter(|event| {
            matches!(
                event["event_type"].as_str(),
                Some("turn_retry" | "turn_edit" | "turn_delete")
            )
        })
        .collect()
}

/// The `event_type` of every audit event enqueued so far, in enqueue order.
pub async fn audit_types(env: &TestEnv) -> Vec<String> {
    env.outbox_payloads(AUDIT_QUEUE)
        .await
        .iter()
        .map(|event| event["event_type"].as_str().unwrap_or_default().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Attachments (S5 owns the upload API; the rows are created with SQL)
// ---------------------------------------------------------------------------

/// A ready document routed to `file_search`.
pub async fn insert_ready_document(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    provider_file_id: &str,
    filename: &str,
) -> Uuid {
    insert_attachment_row(
        env,
        user,
        chat,
        AttachmentSpec {
            kind: "document",
            status: "ready",
            provider_file_id,
            filename,
            content_type: "text/plain",
            for_file_search: true,
            for_code_interpreter: false,
        },
    )
    .await
}

/// A ready image attachment with the given name.
pub async fn insert_named_image(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    provider_file_id: &str,
    filename: &str,
) -> Uuid {
    insert_attachment_row(
        env,
        user,
        chat,
        AttachmentSpec {
            kind: "image",
            status: "ready",
            provider_file_id,
            filename,
            content_type: "image/png",
            for_file_search: false,
            for_code_interpreter: false,
        },
    )
    .await
}

/// The chat's vector store, so that ready documents make `file_search` available.
pub async fn insert_vector_store(env: &TestEnv, user: &TestUser, chat: &str, store: &str) {
    exec_sql(
        env,
        &format!(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, \
             file_count, created_at) VALUES ({}, {}, {}, '{store}', 'openai', 1, {SQL_TS})",
            blob(Uuid::new_v4()),
            blob(user.tenant_id),
            blob(chat_uuid(chat)),
        ),
    )
    .await;
}

/// Links an attachment to a message (the `message_attachments` row of an upload that was sent).
pub async fn link_attachment(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    message: Uuid,
    attachment: Uuid,
) {
    exec_sql(
        env,
        &format!(
            "INSERT INTO message_attachments (tenant_id, chat_id, message_id, attachment_id, \
             created_at) VALUES ({}, {}, {}, {}, {SQL_TS})",
            blob(user.tenant_id),
            blob(chat_uuid(chat)),
            blob(message),
            blob(attachment),
        ),
    )
    .await;
}

/// The attachments linked to `message`, sorted.
pub async fn linked_attachments(env: &TestEnv, chat: &str, message: Uuid) -> Vec<Uuid> {
    let sql = format!(
        "SELECT hex(attachment_id) AS a FROM message_attachments \
         WHERE hex(chat_id) = '{}' AND hex(message_id) = '{}'",
        uuid_hex(chat_uuid(chat)),
        uuid_hex(message)
    );
    let mut ids: Vec<Uuid> = rows(env, sql)
        .await
        .iter()
        .map(|row| opt_uuid(row, "a").unwrap())
        .collect();
    ids.sort();
    ids
}

pub async fn soft_delete_attachment(env: &TestEnv, attachment: Uuid) {
    exec_sql(
        env,
        &format!(
            "UPDATE attachments SET deleted_at = '2000-01-01T00:00:00Z' WHERE hex(id) = '{}'",
            uuid_hex(attachment)
        ),
    )
    .await;
}

/// Sorted copy.
#[must_use]
pub fn sorted(mut ids: Vec<Uuid>) -> Vec<Uuid> {
    ids.sort();
    ids
}

// ---------------------------------------------------------------------------
// Provider request inspection
// ---------------------------------------------------------------------------

/// The `(role, text)` of every `input` item of a Responses request; the text of a multi-part
/// content is the concatenation of its `input_text` parts.
#[must_use]
pub fn input_items(body: &Value) -> Vec<(String, String)> {
    body["input"]
        .as_array()
        .expect("`input` is an array")
        .iter()
        .map(|item| {
            let role = item["role"].as_str().unwrap_or_default().to_owned();
            let text = match &item["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect(),
                other => other.to_string(),
            };
            (role, text)
        })
        .collect()
}

/// Tool types of a request body.
#[must_use]
pub fn tool_types(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool["type"].as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The last chat request the provider got.
#[must_use]
pub fn last_chat_body(env: &TestEnv) -> Value {
    env.provider()
        .chat_requests()
        .last()
        .and_then(|request| request.body_json.clone())
        .expect("a chat request with a JSON body")
}

/// The request id announced by `stream_started`.
#[must_use]
pub fn new_request_id(events: &[SseEvent]) -> Uuid {
    crate::common::stream_request_id(events)
}
