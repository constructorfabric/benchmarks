#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, unused_imports)]

//! Helpers of the slice S4 thread-summary tests (DESIGN section 3.6 "Thread Summary Update",
//! 3.4 "Context Plan Assembly", 3.2 "System Task Attribution Rules", Appendix B.5.5 and B.9.4).
//!
//! The summary work item travels through the shared outbox queue `mini-chat.thread_summary`.
//! Tests drive the handler in two ways:
//!
//! * the real trigger: a turn whose context reaches the compression threshold (or had to drop
//!   messages) enqueues the work item in its finalization transaction;
//! * `enqueue_task`: a hand-made work item enqueued with `TestEnv::enqueue_raw_outbox`, so a
//!   handler stage can be pinned without building the trigger situation.
//!
//! The summary call is the one non-streaming `POST` to the summary model; its answer is scripted
//! with `enqueue_summary*` (FIFO on the `/responses` pattern, after the chat answer of the turn
//! that triggers it).
//!
//! A test file that needs them declares `mod common; mod live; mod mutation; mod billing;
//! mod summary;`.

use mini_chat::test_support::{
    RecordedRequest, ScriptedResponse, TestEnv, TestEnvBuilder, TestUser,
};
use sea_orm::{ConnectionTrait, DbBackend, QueryResult, Statement};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::common::{
    PREMIUM_MODEL, SQL_TS, STANDARD_MODEL, blob, catalog_entry, enqueue_chat, enqueue_text,
    exec_sql, merge, policy, scalar_i64, scalar_text, send_ok, stream_request_id, uuid_hex,
};
use crate::mutation::{Conversation, all_messages, conversation_in};

/// The outbox queue of the thread-summary work items.
pub const SUMMARY_QUEUE: &str = "mini-chat.thread_summary";

/// The catalog model the summary requests go to (`thread_summary_worker.summary_model_id`).
pub const SUMMARY_MODEL: &str = "gpt-summary";
pub const PROVIDER_SUMMARY_MODEL: &str = "provider-gpt-summary";

/// `max_output_tokens` of the summary model's catalog entry.
pub const SUMMARY_MAX_OUTPUT: u64 = 700;

/// The platform default subject id the system identity uses (DESIGN "Provider Request Metadata").
pub const SYSTEM_SUBJECT: &str = "11111111-6a88-4768-9dfc-6bcd5187d9ed";

/// DESIGN B.5.5: the preamble in front of the summary in the next turn's context.
pub const PREAMBLE: &str = "This conversation has earlier messages that have been summarized. \
The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// DESIGN B.5.5: the built-in system prompt of the summary request.
pub const DEFAULT_SUMMARY_PROMPT: &str = "You are a conversation summarizer. Given a conversation \
(and optionally an existing summary), produce a detailed structured summary. Respond with an \
<analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the \
<summary> content will be stored. Do not invent information not present in the conversation.";

/// DESIGN B.5.5: the opening of a summary request without an existing summary.
pub const OPENING_NEW: &str = "Summarize the following conversation:";

/// DESIGN B.5.5: sentences of the opening of an update request (existing summary).
pub const OPENING_UPDATE_1: &str = "The existing summary below covers the earlier conversation. \
Incorporate it with the new messages into a single updated summary.";
pub const OPENING_UPDATE_2: &str = "IMPORTANT: Keep the summary concise. If the combined \
information is too large, prioritize: current topic and recent decisions > user preferences and \
corrections > older facts. Compress or drop the least relevant older details rather than letting \
the summary grow unboundedly.";

/// DESIGN B.5.5: the marker in front of the new messages of an update request.
pub const NEW_MESSAGES_MARKER: &str = "New messages to incorporate:";

/// DESIGN B.5.5: the analysis instruction that ends every summary request (first and last lines
/// plus the five section names).
pub const ANALYSIS_INSTRUCTION_START: &str =
    "Before providing your final summary, wrap your analysis in <analysis> tags.";
pub const ANALYSIS_INSTRUCTION_END: &str =
    "Respond with an <analysis> block followed by a <summary> block.";
pub const SECTIONS: [&str; 5] = [
    "Conversation Purpose",
    "Key Information Exchanged",
    "User Requests and Preferences",
    "Open Items",
    "Current Topic",
];

// ---------------------------------------------------------------------------
// Environments
// ---------------------------------------------------------------------------

/// A builder of environments with a summary model in the catalog.
#[derive(Clone)]
pub struct Setup {
    window: Option<(u32, u32)>,
    worker: Value,
    summary_entry: Value,
    gear: Value,
}

impl Setup {
    /// Premium default model with the normal 128000 token window, a standard model and the
    /// enabled summary model `gpt-summary`; `thread_summary_worker.summary_model_id` names it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            window: None,
            worker: json!({}),
            summary_entry: json!({}),
            gear: json!({}),
        }
    }

    /// The premium (default) model gets a 4000 token window and a 1000 token output cap: input
    /// limit 3000, token budget 2900. A history of a few kilobytes then reaches the compression
    /// threshold or has to be truncated.
    #[must_use]
    pub fn small_window(mut self) -> Self {
        self.window = Some((4000, 1000));
        self
    }

    /// Patch of `thread_summary_worker` (the summary model id is already set).
    #[must_use]
    pub fn worker(mut self, patch: Value) -> Self {
        self.worker = merge(self.worker, patch);
        self
    }

    /// Patch of the summary model's catalog entry.
    #[must_use]
    pub fn summary_entry(mut self, patch: Value) -> Self {
        self.summary_entry = merge(self.summary_entry, patch);
        self
    }

    /// Patch of the gear configuration (outside `thread_summary_worker`).
    #[must_use]
    pub fn gear(mut self, patch: Value) -> Self {
        self.gear = merge(self.gear, patch);
        self
    }

    #[must_use]
    pub fn catalog(&self) -> Vec<Value> {
        let mut premium = catalog_entry(PREMIUM_MODEL, "Premium", true, true);
        if let Some((window, output)) = self.window {
            premium = merge(
                premium,
                json!({ "context_window": window, "max_output_tokens": output }),
            );
        }
        vec![
            premium,
            catalog_entry(STANDARD_MODEL, "Standard", true, false),
            merge(
                catalog_entry(SUMMARY_MODEL, "Standard", true, false),
                merge(
                    json!({ "max_output_tokens": SUMMARY_MAX_OUTPUT }),
                    self.summary_entry.clone(),
                ),
            ),
        ]
    }

    pub async fn build(self) -> TestEnv {
        let worker = merge(
            json!({ "summary_model_id": SUMMARY_MODEL }),
            self.worker.clone(),
        );
        TestEnvBuilder::new()
            .policy_config(policy(self.catalog()))
            .gear_config(merge(
                self.gear.clone(),
                json!({ "thread_summary_worker": worker }),
            ))
            .build()
            .await
    }
}

impl Default for Setup {
    fn default() -> Self {
        Self::new()
    }
}

/// A conversation of completed small turns in a `Setup::new()` environment (no trigger: the
/// window is large).
pub async fn quiet_conversation(pairs: &[(&str, &str)]) -> Conversation {
    conversation_in(Setup::new().build().await, pairs).await
}

/// `marker` followed by `x` up to `bytes` bytes.
#[must_use]
pub fn marked(marker: &str, bytes: usize) -> String {
    format!("{marker}{}", "x".repeat(bytes - marker.len()))
}

/// A user message of 7600 bytes: 2200 estimated tokens (DESIGN 5.5.4 with the `catalog_entry`
/// budgets). In the small window (token budget 2900, effective budget 3000) it fits, and
/// together with the 118 tokens of the system prompt and four small history messages (113 each)
/// the assembled context is 2770 tokens: 92.3 % of the effective budget.
#[must_use]
pub fn big(marker: &str) -> String {
    marked(marker, 7600)
}

/// A user or assistant message of 4000 bytes (1210 estimated tokens).
#[must_use]
pub fn medium(marker: &str) -> String {
    marked(marker, 4000)
}

// ---------------------------------------------------------------------------
// Scripting the summary call
// ---------------------------------------------------------------------------

/// A non-streaming Responses API body with `text` and the given usage.
#[must_use]
pub fn summary_body(
    text: &str,
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
) -> Value {
    json!({
        "id": "resp_summary0123456789abcdef",
        "object": "response",
        "status": "completed",
        "output": [{
            "id": "msg_summary0123456789",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }]
        }],
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": { "cached_tokens": 0 },
            "output_tokens": output_tokens,
            "output_tokens_details": { "reasoning_tokens": reasoning_tokens },
            "total_tokens": input_tokens + output_tokens
        }
    })
}

/// `<analysis>` block followed by a `<summary>` block with `summary`.
#[must_use]
pub fn wrapped(summary: &str) -> String {
    format!(
        "<analysis>\nthinking about the conversation\n</analysis>\n<summary>\n{summary}\n</summary>"
    )
}

/// Queues a successful summary answer (`summary` inside the analysis/summary blocks, usage
/// 120 input / 40 output / 0 reasoning tokens).
pub fn enqueue_summary(env: &TestEnv, summary: &str) {
    enqueue_summary_raw(env, &wrapped(summary), 120, 40, 0);
}

/// Queues a summary answer with exactly `text` and the given usage.
pub fn enqueue_summary_raw(env: &TestEnv, text: &str, input: u64, output: u64, reasoning: u64) {
    enqueue_chat(
        env,
        ScriptedResponse::Json {
            status: 200,
            body: summary_body(text, input, output, reasoning),
        },
    );
}

/// Queues a provider failure for the summary call.
pub fn enqueue_summary_failure(env: &TestEnv, status: u16) {
    enqueue_chat(
        env,
        ScriptedResponse::Error {
            status,
            body: json!({ "error": { "message": "summary provider down" } }),
            headers: vec![],
        },
    );
}

/// Waits (at most 5 s) for `count` handler outcomes of the summary queue.
pub async fn wait_outcomes(env: &TestEnv, count: usize) -> Vec<String> {
    crate::billing::wait_for_outcomes(env, SUMMARY_QUEUE, count).await
}

/// The outcomes as owned strings, for `assert_eq!`.
#[must_use]
pub fn outcomes(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

// ---------------------------------------------------------------------------
// Messages, frontiers and work items
// ---------------------------------------------------------------------------

fn chat_uuid(chat: &str) -> Uuid {
    Uuid::parse_str(chat).expect("chat id must be a UUID")
}

/// A position in the per-chat message order `(created_at, id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontier {
    /// RFC 3339.
    pub created_at: String,
    pub message_id: Uuid,
}

impl Frontier {
    #[must_use]
    pub fn instant(&self) -> OffsetDateTime {
        OffsetDateTime::parse(&self.created_at, &Rfc3339).unwrap()
    }
}

/// The `(created_at, id)` of a message.
pub async fn frontier_of(env: &TestEnv, message: Uuid) -> Frontier {
    let text = scalar_text(
        env,
        &format!(
            "SELECT CAST(created_at AS TEXT) AS t FROM messages WHERE hex(id) = '{}'",
            uuid_hex(message)
        ),
    )
    .await;
    let parsed = OffsetDateTime::parse(&text, &Rfc3339)
        .unwrap_or_else(|err| panic!("`{text}` is not an RFC 3339 timestamp: {err}"));
    Frontier {
        created_at: parsed.format(&Rfc3339).unwrap(),
        message_id: message,
    }
}

/// The id of the `index`-th message of the chat in conversation order (soft-deleted ones
/// included): turn `n` (0-based) has its user message at `2n` and its answer at `2n + 1`.
pub async fn message_at(env: &TestEnv, chat: &str, index: usize) -> Uuid {
    all_messages(env, chat).await[index].id
}

/// The JSON of a work item as the gear serializes it (DESIGN "Durable scheduling").
#[must_use]
pub fn task_payload(
    user: &TestUser,
    chat: &str,
    system_request_id: Uuid,
    base: Option<&Frontier>,
    target: &Frontier,
) -> Value {
    json!({
        "tenant_id": user.tenant_id,
        "chat_id": chat,
        "system_request_id": system_request_id,
        "base_frontier_created_at": base.map(|b| b.created_at.clone()),
        "base_frontier_message_id": base.map(|b| b.message_id),
        "frozen_target_created_at": target.created_at,
        "frozen_target_message_id": target.message_id,
        "system_task_type": "thread_summary_update",
    })
}

/// Enqueues a hand-made work item (partitioned by the chat like the gear's own) and returns its
/// `system_request_id`.
pub async fn enqueue_task(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    base: Option<&Frontier>,
    target: &Frontier,
) -> Uuid {
    let system_request_id = Uuid::new_v4();
    let payload = task_payload(user, chat, system_request_id, base, target);
    env.enqueue_raw_outbox(
        SUMMARY_QUEUE,
        chat_uuid(chat),
        payload.to_string().as_bytes(),
    )
    .await;
    system_request_id
}

/// Enqueues a work item summarizing everything up to the `index`-th message (no base) and
/// returns its `system_request_id`.
pub async fn enqueue_task_up_to(env: &TestEnv, user: &TestUser, chat: &str, index: usize) -> Uuid {
    let target = frontier_of(env, message_at(env, chat, index).await).await;
    enqueue_task(env, user, chat, None, &target).await
}

/// The work items the gear itself enqueued so far (hand-made ones are not listed).
pub async fn tasks(env: &TestEnv) -> Vec<Value> {
    env.outbox_payloads(SUMMARY_QUEUE).await
}

// ---------------------------------------------------------------------------
// Stored state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SummaryRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub text: String,
    pub frontier: Frontier,
    pub token_estimate: i64,
}

async fn query(env: &TestEnv, sql: String) -> Vec<QueryResult> {
    env.db()
        .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
        .expect("row query must succeed")
}

fn uuid_of(row: &QueryResult, column: &str) -> Uuid {
    let hex: String = row.try_get("", column).unwrap();
    Uuid::parse_str(&hex).unwrap()
}

/// The chat's `thread_summaries` row.
pub async fn summary_row(env: &TestEnv, chat: &str) -> Option<SummaryRow> {
    let sql = format!(
        "SELECT hex(id) AS id_hex, hex(tenant_id) AS tenant_hex, summary_text, \
         CAST(summarized_up_to_created_at AS TEXT) AS at, \
         hex(summarized_up_to_message_id) AS message_hex, token_estimate \
         FROM thread_summaries WHERE hex(chat_id) = '{}'",
        uuid_hex(chat_uuid(chat))
    );
    let rows = query(env, sql).await;
    assert!(rows.len() <= 1, "UNIQUE(chat_id): {} rows", rows.len());
    rows.first().map(|row| {
        let text: String = row.try_get("", "at").unwrap();
        let parsed =
            OffsetDateTime::parse(&text, &Rfc3339).unwrap_or_else(|err| panic!("`{text}`: {err}"));
        SummaryRow {
            id: uuid_of(row, "id_hex"),
            tenant_id: uuid_of(row, "tenant_hex"),
            text: row.try_get("", "summary_text").unwrap(),
            frontier: Frontier {
                created_at: parsed.format(&Rfc3339).unwrap(),
                message_id: uuid_of(row, "message_hex"),
            },
            token_estimate: row.try_get("", "token_estimate").unwrap(),
        }
    })
}

/// Number of `thread_summaries` rows of every chat.
pub async fn summary_row_count(env: &TestEnv) -> i64 {
    scalar_i64(env, "SELECT COUNT(*) AS n FROM thread_summaries").await
}

/// The ids of the messages flagged `is_compressed`, in conversation order.
pub async fn compressed_ids(env: &TestEnv, chat: &str) -> Vec<Uuid> {
    all_messages(env, chat)
        .await
        .into_iter()
        .filter(|message| message.compressed)
        .map(|message| message.id)
        .collect()
}

/// The ids of the first `count` messages of the chat (conversation order, soft-deleted ones
/// included).
pub async fn first_messages(env: &TestEnv, chat: &str, count: usize) -> Vec<Uuid> {
    all_messages(env, chat)
        .await
        .into_iter()
        .take(count)
        .map(|message| message.id)
        .collect()
}

/// Seeds the chat's summary as an earlier run left it: the row, and `is_compressed` on every
/// message up to and including the `frontier_index`-th one.
pub async fn seed_summary(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    text: &str,
    frontier_index: usize,
    token_estimate: i64,
) -> Frontier {
    let messages = all_messages(env, chat).await;
    let frontier = frontier_of(env, messages[frontier_index].id).await;
    exec_sql(
        env,
        &format!(
            "INSERT INTO thread_summaries (id, tenant_id, chat_id, summary_text, \
             summarized_up_to_created_at, summarized_up_to_message_id, token_estimate, \
             created_at, updated_at) VALUES ({}, {}, {}, '{}', '{}', {}, {token_estimate}, {SQL_TS}, {SQL_TS})",
            blob(Uuid::new_v4()),
            blob(user.tenant_id),
            blob(chat_uuid(chat)),
            text.replace('\'', "''"),
            frontier.created_at,
            blob(frontier.message_id),
        ),
    )
    .await;
    for message in messages.iter().take(frontier_index + 1) {
        exec_sql(
            env,
            &format!(
                "UPDATE messages SET is_compressed = 1 WHERE hex(id) = '{}'",
                uuid_hex(message.id)
            ),
        )
        .await;
    }
    frontier
}

// ---------------------------------------------------------------------------
// The summary request as the provider saw it
// ---------------------------------------------------------------------------

/// Every provider request that is a summary request (`metadata.request_type == "summary"`).
#[must_use]
pub fn summary_requests(env: &TestEnv) -> Vec<RecordedRequest> {
    env.provider()
        .chat_requests()
        .into_iter()
        .filter(|request| {
            request
                .body_json
                .as_ref()
                .is_some_and(|body| body["metadata"]["request_type"] == json!("summary"))
        })
        .collect()
}

/// The body of the only summary request.
#[must_use]
pub fn summary_request(env: &TestEnv) -> Value {
    let requests = summary_requests(env);
    assert_eq!(
        requests.len(),
        1,
        "exactly one summary request: {requests:?}"
    );
    requests[0].body_json.clone().unwrap()
}

/// The user prompt: the text of every `input` item.
#[must_use]
pub fn user_prompt(body: &Value) -> String {
    crate::mutation::input_items(body)
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The chat requests that are not summary requests.
#[must_use]
pub fn turn_requests(env: &TestEnv) -> Vec<RecordedRequest> {
    env.provider()
        .chat_requests()
        .into_iter()
        .filter(|request| {
            request
                .body_json
                .as_ref()
                .is_none_or(|body| body["metadata"]["request_type"] != json!("summary"))
        })
        .collect()
}

/// The usage events of system tasks enqueued so far.
pub async fn system_usage_events(env: &TestEnv) -> Vec<Value> {
    env.outbox_payloads(crate::common::USAGE_QUEUE)
        .await
        .into_iter()
        .filter(|event| event["billing_outcome"] == json!("system_task"))
        .collect()
}
