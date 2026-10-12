#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, unused_imports)]

//! Shared helpers for the mini-chat black-box integration tests.
//!
//! Every helper talks to the gear only through `mini_chat::test_support` (the harness
//! contract) and the public HTTP surface. Direct database assertions use raw SQL so the
//! tests do not depend on entity module paths. Each test binary uses a subset of these
//! helpers, hence the lint allowances above.

use std::fmt::Write as _;

use axum::body::Body;
use http::{Method, Request, StatusCode};
use mini_chat::test_support::{
    AuthzMode, ScriptedResponse, SseEvent, TestEnv, TestEnvBuilder, TestResponse, TestUser,
};
use sea_orm::{ConnectionTrait, DbBackend, QueryResult, Statement};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

pub const CHATS_PATH: &str = "/mini-chat/v1/chats";
pub const MODELS_PATH: &str = "/mini-chat/v1/models";

pub const CHAT_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.chat.v1~";
pub const MODEL_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.model.v1~";
pub const ODATA_RESOURCE_TYPE: &str = "gts.cf.core.odata.query.v1~";

// Harness default catalog (see HARNESS.md): the default model, a second enabled model
// and a disabled one.
pub const PREMIUM_MODEL: &str = "gpt-premium";
pub const STANDARD_MODEL: &str = "gpt-standard";
pub const DISABLED_MODEL: &str = "gpt-disabled";

// ---------------------------------------------------------------------------
// Paths and query strings
// ---------------------------------------------------------------------------

#[must_use]
pub fn chat_path(chat_id: &str) -> String {
    format!("{CHATS_PATH}/{chat_id}")
}

#[must_use]
pub fn model_path(model_id: &str) -> String {
    format!("{MODELS_PATH}/{model_id}")
}

/// Percent-encodes everything except the RFC 3986 unreserved characters.
#[must_use]
pub fn url_encode(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            write!(out, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    out
}

/// Builds `chats?k=v&...` with every key and value percent-encoded.
#[must_use]
pub fn chats_url(params: &[(&str, &str)]) -> String {
    if params.is_empty() {
        return CHATS_PATH.to_owned();
    }
    let query: Vec<String> = params
        .iter()
        .map(|(key, value)| format!("{}={}", url_encode(key), url_encode(value)))
        .collect();
    format!("{CHATS_PATH}?{}", query.join("&"))
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

pub async fn get(env: &TestEnv, user: &TestUser, path: &str) -> TestResponse {
    env.request(user, Method::GET, path, None).await
}

pub async fn post_chat(env: &TestEnv, user: &TestUser, body: Value) -> TestResponse {
    env.request(user, Method::POST, CHATS_PATH, Some(body))
        .await
}

pub async fn get_chat(env: &TestEnv, user: &TestUser, chat_id: &str) -> TestResponse {
    get(env, user, &chat_path(chat_id)).await
}

pub async fn patch_chat(
    env: &TestEnv,
    user: &TestUser,
    chat_id: &str,
    body: Value,
) -> TestResponse {
    env.request(user, Method::PATCH, &chat_path(chat_id), Some(body))
        .await
}

pub async fn delete_chat(env: &TestEnv, user: &TestUser, chat_id: &str) -> TestResponse {
    env.request(user, Method::DELETE, &chat_path(chat_id), None)
        .await
}

pub async fn list_chats(env: &TestEnv, user: &TestUser, params: &[(&str, &str)]) -> TestResponse {
    get(env, user, &chats_url(params)).await
}

/// Lists chats and asserts a 200 response; returns the JSON page.
pub async fn list_chats_ok(env: &TestEnv, user: &TestUser, params: &[(&str, &str)]) -> Value {
    let resp = list_chats(env, user, params).await;
    expect_status(&resp, StatusCode::OK);
    resp.json()
}

/// A request with an explicit content type and a raw (possibly malformed) body.
#[must_use]
pub fn raw_request(
    method: Method,
    path: &str,
    content_type: Option<&str>,
    body: &str,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(content_type) = content_type {
        builder = builder.header(http::header::CONTENT_TYPE, content_type);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

// ---------------------------------------------------------------------------
// Chat creation helpers
// ---------------------------------------------------------------------------

/// Creates a chat and asserts 201; returns the `ChatDetail` JSON.
pub async fn create_chat(env: &TestEnv, user: &TestUser, body: Value) -> Value {
    let resp = post_chat(env, user, body).await;
    expect_status(&resp, StatusCode::CREATED);
    resp.json()
}

pub async fn create_default_chat(env: &TestEnv, user: &TestUser) -> Value {
    create_chat(env, user, json!({})).await
}

pub async fn create_titled_chat(env: &TestEnv, user: &TestUser, title: &str) -> Value {
    create_chat(env, user, json!({ "title": title })).await
}

/// Creates one titled chat per title, in order; returns their ids in the same order.
pub async fn create_titled_chats(env: &TestEnv, user: &TestUser, titles: &[&str]) -> Vec<String> {
    let mut ids = Vec::new();
    for title in titles {
        ids.push(chat_id(&create_titled_chat(env, user, title).await));
    }
    ids
}

#[must_use]
pub fn chat_id(chat: &Value) -> String {
    chat["id"]
        .as_str()
        .expect("chat detail must carry a string id")
        .to_owned()
}

// ---------------------------------------------------------------------------
// Response assertions
// ---------------------------------------------------------------------------

#[must_use]
pub fn body_text(resp: &TestResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

pub fn expect_status(resp: &TestResponse, expected: StatusCode) {
    assert_eq!(
        resp.status,
        expected,
        "unexpected status, body: {}",
        body_text(resp)
    );
}

/// Asserts a canonical `Problem` (ADR-0004): status, `application/problem+json`, the
/// `type`/`title`/`status`/`detail`/`context` members, no top-level `code`, and optionally
/// the canonical category (the `cf.core.err.<category>.v1~` suffix of `type`).
/// Returns the parsed body for further assertions.
pub fn assert_problem(resp: &TestResponse, status: u16, category: Option<&str>) -> Value {
    assert_eq!(
        resp.status.as_u16(),
        status,
        "unexpected status, body: {}",
        body_text(resp)
    );
    let content_type = resp
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("application/problem+json"),
        "error responses must be application/problem+json, got: {content_type}"
    );
    let body = resp.json();
    let object = body.as_object().expect("a problem body is a JSON object");
    for member in ["type", "title", "status", "detail", "context"] {
        assert!(
            object.contains_key(member),
            "problem is missing `{member}`: {body}"
        );
    }
    assert!(
        !object.contains_key("code"),
        "a canonical problem has no top-level `code`: {body}"
    );
    assert_eq!(body["status"].as_u64(), Some(u64::from(status)), "{body}");
    assert!(body["context"].is_object(), "context must be an object");
    if let Some(category) = category {
        let suffix = format!("cf.core.err.{category}.v1~");
        let problem_type = body["type"].as_str().expect("`type` must be a string");
        assert!(
            problem_type.ends_with(&suffix),
            "expected category `{category}`, got type `{problem_type}`"
        );
    }
    body
}

/// The `reason` of the first `context.field_violations` entry whose `field` matches.
#[must_use]
pub fn field_violation_reason(problem: &Value, field: &str) -> Option<String> {
    problem["context"]["field_violations"]
        .as_array()?
        .iter()
        .find(|violation| violation["field"].as_str() == Some(field))
        .and_then(|violation| violation["reason"].as_str())
        .map(str::to_owned)
}

/// True when any `context.field_violations` entry carries `reason`.
#[must_use]
pub fn has_violation_reason(problem: &Value, reason: &str) -> bool {
    problem["context"]["field_violations"]
        .as_array()
        .is_some_and(|violations| {
            violations
                .iter()
                .any(|violation| violation["reason"].as_str() == Some(reason))
        })
}

/// Asserts a 404 `not_found` problem naming `resource_type`.
pub fn assert_not_found(resp: &TestResponse, resource_type: &str) -> Value {
    let problem = assert_problem(resp, 404, Some("not_found"));
    assert_eq!(
        problem["context"]["resource_type"].as_str(),
        Some(resource_type),
        "{problem}"
    );
    problem
}

/// Asserts a 400 `invalid_argument` problem with `field_violations[field].reason == reason`.
pub fn assert_invalid_argument(resp: &TestResponse, field: &str, reason: &str) -> Value {
    let problem = assert_problem(resp, 400, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, field).as_deref(),
        Some(reason),
        "expected field_violations[{field}].reason = {reason}: {problem}"
    );
    problem
}

/// Asserts the `ChatDetail` wire shape: exactly the documented members, no identity
/// leakage, `title` only when `expect_title` is true.
pub fn assert_chat_detail_shape(chat: &Value, expect_title: bool) {
    let object = chat.as_object().expect("ChatDetail is a JSON object");
    let mut expected = vec![
        "id",
        "model",
        "is_temporary",
        "message_count",
        "created_at",
        "updated_at",
    ];
    if expect_title {
        expected.push("title");
    }
    for member in &expected {
        assert!(object.contains_key(*member), "missing `{member}`: {chat}");
    }
    for key in object.keys() {
        assert!(
            expected.contains(&key.as_str()),
            "unexpected member `{key}` in ChatDetail: {chat}"
        );
    }
    assert!(
        Uuid::parse_str(chat["id"].as_str().unwrap()).is_ok(),
        "{chat}"
    );
    assert!(chat["model"].is_string(), "{chat}");
    assert_eq!(chat["is_temporary"], json!(false), "{chat}");
    assert!(chat["message_count"].is_u64(), "{chat}");
    assert!(
        parse_ts(&chat["created_at"]) <= parse_ts(&chat["updated_at"]),
        "created_at must not be after updated_at: {chat}"
    );
}

#[must_use]
pub fn parse_ts(value: &Value) -> OffsetDateTime {
    OffsetDateTime::parse(
        value.as_str().expect("timestamp must be a string"),
        &Rfc3339,
    )
    .expect("timestamp must be RFC 3339")
}

// ---------------------------------------------------------------------------
// List page helpers
// ---------------------------------------------------------------------------

#[must_use]
pub fn items(page: &Value) -> &Vec<Value> {
    page["items"].as_array().expect("`items` must be an array")
}

#[must_use]
pub fn item_ids(page: &Value) -> Vec<String> {
    items(page).iter().map(chat_id).collect()
}

/// Titles of the listed chats; an untitled chat yields an empty string.
#[must_use]
pub fn item_titles(page: &Value) -> Vec<String> {
    items(page)
        .iter()
        .map(|item| item["title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[must_use]
pub fn next_cursor(page: &Value) -> Option<String> {
    page["page_info"]["next_cursor"].as_str().map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Direct database assertions (raw SQL, SQLite)
// ---------------------------------------------------------------------------

/// UUID columns are stored as 16-byte BLOBs; SQLite `hex()` renders them upper-case.
#[must_use]
pub fn uuid_hex(id: Uuid) -> String {
    id.simple().to_string().to_uppercase()
}

pub struct ChatRow {
    pub tenant_hex: String,
    pub user_hex: String,
    pub model: String,
    pub title: Option<String>,
    pub deleted: bool,
}

/// The raw `chats` row (soft-deleted rows included), or `None` when there is no such row.
pub async fn chat_row(env: &TestEnv, chat_id: &str) -> Option<ChatRow> {
    let id = Uuid::parse_str(chat_id).expect("chat id must be a UUID");
    let sql = format!(
        "SELECT hex(tenant_id) AS tenant_hex, hex(user_id) AS user_hex, model, title, \
         CASE WHEN deleted_at IS NULL THEN 0 ELSE 1 END AS is_deleted \
         FROM chats WHERE hex(id) = '{}'",
        uuid_hex(id)
    );
    let db = env.db();
    let row = db
        .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
        .expect("chats row query must succeed")?;
    Some(ChatRow {
        tenant_hex: row.try_get("", "tenant_hex").unwrap(),
        user_hex: row.try_get("", "user_hex").unwrap(),
        model: row.try_get("", "model").unwrap(),
        title: row.try_get("", "title").unwrap(),
        deleted: row.try_get::<i64>("", "is_deleted").unwrap() != 0,
    })
}

/// Number of rows in `chats`, soft-deleted ones included.
pub async fn chat_row_count(env: &TestEnv) -> i64 {
    let db = env.db();
    let row = db
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM chats".to_owned(),
        ))
        .await
        .expect("count query must succeed")
        .expect("COUNT(*) always returns a row");
    row.try_get("", "n").unwrap()
}

// ---------------------------------------------------------------------------
// Environments and model catalogs
// ---------------------------------------------------------------------------

pub async fn env_with_authz(mode: AuthzMode) -> TestEnv {
    TestEnvBuilder::new().authz(mode).build().await
}

/// An environment whose static policy plugin serves exactly `entries`.
pub async fn env_with_catalog(entries: Vec<Value>) -> TestEnv {
    TestEnvBuilder::new()
        .policy_config(json!({ "model_catalog": entries }))
        .build()
        .await
}

/// A full `ModelCatalogEntry` in the shape of `config/mini-chat.yaml`
/// (`tier` is `"Premium"` or `"Standard"`; provider is the harness provider `openai`).
#[must_use]
pub fn catalog_entry(id: &str, tier: &str, enabled: bool, is_default: bool) -> Value {
    json!({
        "id": id,
        "provider_model_id": format!("provider-{id}"),
        "display_name": format!("Display {id}"),
        "description": format!("Description of {id}"),
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "icon": "",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": "You are a helpful assistant.",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": "1x",
        "estimation_budgets": {
            "bytes_per_token_conservative": 4,
            "fixed_overhead_tokens": 100,
            "safety_margin_pct": 10,
            "image_token_budget": 1000,
            "tool_surcharge_tokens": 500,
            "web_search_surcharge_tokens": 500,
            "code_interpreter_surcharge_tokens": 1000,
            "minimal_generation_floor": 50
        },
        "max_num_results": 5,
        "web_search_context_size": "low",
        "max_tool_calls": 2,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": {
                "web_search": true,
                "file_search": true,
                "image_generation": false,
                "code_interpreter": true,
                "mcp": false
            },
            "supported_endpoints": { "chat_completions": true, "responses": true }
        },
        "preference": { "is_default": is_default, "sort_order": 0 }
    })
}

/// Drops the optional `description` from a catalog entry.
#[must_use]
pub fn without_description(mut entry: Value) -> Value {
    entry
        .as_object_mut()
        .expect("catalog entry is an object")
        .remove("description");
    entry
}

// ===========================================================================
// Slice S2 (send pipeline) helpers
//
// Everything below uses only the harness API that exists today (`request`, `provider`, `db`,
// `outbox_payloads`). Helpers that need the S2-only harness API (`request_streaming`,
// `wait_for_turn_state`, `drain_outbox`, ...) live in `tests/live/mod.rs`, so the S1 test
// binaries keep compiling.
// ===========================================================================

pub const MESSAGE_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.message.v1~";
pub const TURN_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.turn.v1~";

/// Outbox queues of the gear (DESIGN section 5.6, B.1).
pub const USAGE_QUEUE: &str = "mini-chat.usage_snapshot";
pub const AUDIT_QUEUE: &str = "mini-chat.audit";

/// The system prompt of every `catalog_entry` and of the harness default models.
pub const SYSTEM_PROMPT: &str = "You are a helpful assistant.";

/// A provider response id as the real `OpenAI` Responses API issues it. It must never reach a client.
pub const PROVIDER_RESPONSE_ID: &str = "resp_0123456789abcdef0123";

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

#[must_use]
pub fn stream_path(chat_id: &str) -> String {
    format!("{CHATS_PATH}/{chat_id}/messages:stream")
}

#[must_use]
pub fn messages_path(chat_id: &str) -> String {
    format!("{CHATS_PATH}/{chat_id}/messages")
}

/// `messages?k=v&...` with every key and value percent-encoded.
#[must_use]
pub fn messages_url(chat_id: &str, params: &[(&str, &str)]) -> String {
    if params.is_empty() {
        return messages_path(chat_id);
    }
    let query: Vec<String> = params
        .iter()
        .map(|(key, value)| format!("{}={}", url_encode(key), url_encode(value)))
        .collect();
    format!("{}?{}", messages_path(chat_id), query.join("&"))
}

#[must_use]
pub fn turn_path(chat_id: &str, request_id: &str) -> String {
    format!("{CHATS_PATH}/{chat_id}/turns/{request_id}")
}

#[must_use]
pub fn reaction_path(chat_id: &str, message_id: &str) -> String {
    format!("{CHATS_PATH}/{chat_id}/messages/{message_id}/reaction")
}

// ---------------------------------------------------------------------------
// Environments and catalogs
// ---------------------------------------------------------------------------

/// Deep-merges `patch` over `base` (objects merge key by key, everything else is replaced).
#[must_use]
pub fn merge(mut base: Value, patch: Value) -> Value {
    fn go(base: &mut Value, patch: Value) {
        match (base, patch) {
            (Value::Object(base_map), Value::Object(patch_map)) => {
                for (key, value) in patch_map {
                    match base_map.get_mut(&key) {
                        Some(existing) => go(existing, value),
                        None => {
                            base_map.insert(key, value);
                        }
                    }
                }
            }
            (slot, value) => *slot = value,
        }
    }
    go(&mut base, patch);
    base
}

/// A premium default model (`gpt-premium`) and a standard model (`gpt-standard`) built with
/// `catalog_entry`: `provider_model_id` is `provider-<id>`, multipliers `1_000_000` / `3_000_000`,
/// `max_output_tokens` 4096, `context_window` 128000, `max_input_tokens` 0, no tool restrictions.
#[must_use]
pub fn two_model_catalog() -> Vec<Value> {
    vec![
        catalog_entry(PREMIUM_MODEL, "Premium", true, true),
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]
}

/// A `policy_config` with `entries` as the model catalog and the plugin defaults otherwise.
#[must_use]
pub fn policy(entries: Vec<Value>) -> Value {
    json!({ "model_catalog": Value::Array(entries) })
}

/// Per-user limits (daily and monthly) that no request can fit into.
#[must_use]
pub fn tiny_limits() -> Value {
    json!({ "limit_daily_credits_micro": 1000, "limit_monthly_credits_micro": 1000 })
}

#[must_use]
pub fn limits(daily: i64, monthly: i64) -> Value {
    json!({ "limit_daily_credits_micro": daily, "limit_monthly_credits_micro": monthly })
}

pub async fn env_with_policy(policy: Value) -> TestEnv {
    TestEnvBuilder::new().policy_config(policy).build().await
}

/// An environment serving `two_model_catalog()`.
pub async fn env_two_models() -> TestEnv {
    env_with_policy(policy(two_model_catalog())).await
}

/// An environment with the two-model catalog and `patch` merged over the gear configuration.
pub async fn env_two_models_with_config(patch: Value) -> TestEnv {
    TestEnvBuilder::new()
        .policy_config(policy(two_model_catalog()))
        .gear_config(patch)
        .build()
        .await
}

/// Creates a chat on the default model and returns its id.
pub async fn new_chat(env: &TestEnv, user: &TestUser) -> String {
    chat_id(&create_default_chat(env, user).await)
}

/// A default environment, a user and an empty chat.
pub async fn env_user_chat() -> (TestEnv, TestUser, String) {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

// ---------------------------------------------------------------------------
// Scripted provider streams (OpenAI Responses API event shapes)
// ---------------------------------------------------------------------------

pub type Frames = Vec<(String, Value)>;

#[must_use]
pub fn frame(name: &str, data: Value) -> (String, Value) {
    let mut data = data;
    if let Some(object) = data.as_object_mut() {
        object
            .entry("type")
            .or_insert_with(|| Value::String(name.to_owned()));
    }
    (name.to_owned(), data)
}

fn response_object(status: &str, usage: Option<(u64, u64)>, output: Value) -> Value {
    let mut response = json!({
        "id": PROVIDER_RESPONSE_ID,
        "object": "response",
        "status": status,
    });
    response["output"] = output;
    if let Some((input_tokens, output_tokens)) = usage {
        response["usage"] = json!({
            "input_tokens": input_tokens,
            "input_tokens_details": { "cached_tokens": 0 },
            "output_tokens": output_tokens,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": input_tokens + output_tokens,
        });
    }
    response
}

/// The frames the provider sends before the first text delta.
#[must_use]
pub fn opening_frames() -> Frames {
    vec![
        frame(
            "response.created",
            json!({ "response": response_object("in_progress", None, json!([])), "sequence_number": 0 }),
        ),
        frame(
            "response.in_progress",
            json!({ "response": response_object("in_progress", None, json!([])), "sequence_number": 1 }),
        ),
        frame(
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": { "id": "msg_0123456789abcdef", "type": "message", "status": "in_progress",
                          "role": "assistant", "content": [] },
                "sequence_number": 2
            }),
        ),
        frame(
            "response.content_part.added",
            json!({
                "item_id": "msg_0123456789abcdef", "output_index": 0, "content_index": 0,
                "part": { "type": "output_text", "text": "", "annotations": [] },
                "sequence_number": 3
            }),
        ),
    ]
}

/// One `response.output_text.delta` frame per chunk.
#[must_use]
pub fn delta_frames(chunks: &[&str]) -> Frames {
    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            frame(
                "response.output_text.delta",
                json!({
                    "item_id": "msg_0123456789abcdef", "output_index": 0, "content_index": 0,
                    "delta": chunk, "sequence_number": 4 + index
                }),
            )
        })
        .collect()
}

/// The frames that close a successful answer with the given `annotations` on the text part,
/// ending with `final_event` (`response.completed` or `response.incomplete`).
#[must_use]
pub fn closing_frames(
    text: &str,
    annotations: &Value,
    usage: Option<(u64, u64)>,
    final_event: &str,
) -> Frames {
    let part = json!({ "type": "output_text", "text": text, "annotations": annotations });
    let item = json!({
        "id": "msg_0123456789abcdef", "type": "message", "status": "completed",
        "role": "assistant", "content": [part]
    });
    let status = if final_event == "response.incomplete" {
        "incomplete"
    } else {
        "completed"
    };
    vec![
        frame(
            "response.output_text.done",
            json!({
                "item_id": "msg_0123456789abcdef", "output_index": 0, "content_index": 0,
                "text": text, "sequence_number": 90
            }),
        ),
        frame(
            "response.content_part.done",
            json!({
                "item_id": "msg_0123456789abcdef", "output_index": 0, "content_index": 0,
                "part": part, "sequence_number": 91
            }),
        ),
        frame(
            "response.output_item.done",
            json!({ "output_index": 0, "item": item, "sequence_number": 92 }),
        ),
        frame(
            final_event,
            json!({
                "response": response_object(status, usage, json!([item])),
                "sequence_number": 93
            }),
        ),
    ]
}

/// A complete successful answer: opening frames, one delta per chunk, closing frames.
#[must_use]
pub fn answer_frames(chunks: &[&str], input_tokens: u64, output_tokens: u64) -> Frames {
    let mut frames = opening_frames();
    frames.extend(delta_frames(chunks));
    frames.extend(closing_frames(
        &chunks.concat(),
        &json!([]),
        Some((input_tokens, output_tokens)),
        "response.completed",
    ));
    frames
}

/// A successful answer that ends with `response.incomplete`.
#[must_use]
pub fn incomplete_frames(chunks: &[&str], input_tokens: u64, output_tokens: u64) -> Frames {
    let mut frames = opening_frames();
    frames.extend(delta_frames(chunks));
    frames.extend(closing_frames(
        &chunks.concat(),
        &json!([]),
        Some((input_tokens, output_tokens)),
        "response.incomplete",
    ));
    frames
}

/// The frames of an answer that has started streaming but is still in progress.
#[must_use]
pub fn partial_frames(chunks: &[&str]) -> Frames {
    let mut frames = opening_frames();
    frames.extend(delta_frames(chunks));
    frames
}

/// A complete answer as a scripted provider response.
#[must_use]
pub fn text_script(chunks: &[&str], input_tokens: u64, output_tokens: u64) -> ScriptedResponse {
    ScriptedResponse::Sse(answer_frames(chunks, input_tokens, output_tokens))
}

/// An answer that stays open after `chunks` until the client disconnects.
#[must_use]
pub fn hang_script(chunks: &[&str]) -> ScriptedResponse {
    ScriptedResponse::SseThenHang(partial_frames(chunks))
}

/// A stream that fails with `response.failed` carrying `message` (and `usage` when given).
#[must_use]
pub fn failed_script(message: &str, usage: Option<(u64, u64)>) -> ScriptedResponse {
    let mut response = response_object("failed", usage, json!([]));
    response["error"] = json!({ "code": "server_error", "message": message });
    let mut frames = opening_frames();
    frames.push(frame(
        "response.failed",
        json!({ "response": response, "sequence_number": 10 }),
    ));
    ScriptedResponse::Sse(frames)
}

/// Queues a response for the next chat request of the provider.
pub fn enqueue_chat(env: &TestEnv, response: ScriptedResponse) {
    env.provider().enqueue("/responses", response);
}

/// Queues a complete text answer for the next chat request.
pub fn enqueue_text(env: &TestEnv, chunks: &[&str], input_tokens: u64, output_tokens: u64) {
    enqueue_chat(env, text_script(chunks, input_tokens, output_tokens));
}

// ---------------------------------------------------------------------------
// Sending messages and reading SSE
// ---------------------------------------------------------------------------

pub async fn send(env: &TestEnv, user: &TestUser, chat_id: &str, body: Value) -> TestResponse {
    env.request(user, Method::POST, &stream_path(chat_id), Some(body))
        .await
}

pub async fn send_text(
    env: &TestEnv,
    user: &TestUser,
    chat_id: &str,
    content: &str,
) -> TestResponse {
    send(env, user, chat_id, json!({ "content": content })).await
}

pub async fn send_with_id(
    env: &TestEnv,
    user: &TestUser,
    chat_id: &str,
    content: &str,
    request_id: Uuid,
) -> TestResponse {
    send(
        env,
        user,
        chat_id,
        json!({ "content": content, "request_id": request_id.to_string() }),
    )
    .await
}

#[must_use]
pub fn content_type(resp: &TestResponse) -> String {
    resp.headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Asserts a 200 `text/event-stream` response and returns its events without `ping` keepalives.
pub fn expect_stream(resp: &TestResponse) -> Vec<SseEvent> {
    expect_status(resp, StatusCode::OK);
    let content_type = content_type(resp);
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected text/event-stream, got `{content_type}`: {}",
        body_text(resp)
    );
    without_pings(resp.sse_events())
}

#[must_use]
pub fn without_pings(events: Vec<SseEvent>) -> Vec<SseEvent> {
    events
        .into_iter()
        .filter(|event| event.event != "ping")
        .collect()
}

/// Sends a message and asserts a stream that ends with `done`.
pub async fn send_ok(
    env: &TestEnv,
    user: &TestUser,
    chat_id: &str,
    content: &str,
) -> Vec<SseEvent> {
    let events = expect_stream(&send_text(env, user, chat_id, content).await);
    assert_eq!(
        terminal(&events).event,
        "done",
        "expected a completed stream: {events:?}"
    );
    events
}

#[must_use]
pub fn names(events: &[SseEvent]) -> Vec<&str> {
    events.iter().map(|event| event.event.as_str()).collect()
}

#[must_use]
pub fn named<'a>(events: &'a [SseEvent], name: &str) -> Vec<&'a SseEvent> {
    events.iter().filter(|event| event.event == name).collect()
}

/// The first event called `name`.
pub fn first<'a>(events: &'a [SseEvent], name: &str) -> &'a SseEvent {
    named(events, name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no `{name}` event in {events:?}"))
}

/// `stream_started` (the first event of every stream).
pub fn started(events: &[SseEvent]) -> &SseEvent {
    let first = events.first().expect("the stream carries events");
    assert_eq!(first.event, "stream_started", "{events:?}");
    first
}

/// The concatenated `content` of every `delta` event.
#[must_use]
pub fn streamed_text(events: &[SseEvent]) -> String {
    named(events, "delta")
        .iter()
        .map(|event| event.data["content"].as_str().unwrap_or_default())
        .collect()
}

/// The terminal event: the last one, and the only `done` / `error`.
pub fn terminal(events: &[SseEvent]) -> &SseEvent {
    let terminals = events
        .iter()
        .filter(|event| event.event == "done" || event.event == "error")
        .count();
    assert_eq!(
        terminals, 1,
        "exactly one terminal event expected: {events:?}"
    );
    let last = events.last().expect("the stream carries events");
    assert!(
        last.event == "done" || last.event == "error",
        "the terminal event must be last: {events:?}"
    );
    last
}

/// The `request_id` of `stream_started`, parsed.
pub fn stream_request_id(events: &[SseEvent]) -> Uuid {
    Uuid::parse_str(started(events).data["request_id"].as_str().unwrap()).unwrap()
}

/// The pre-allocated assistant `message_id` of `stream_started`.
pub fn stream_message_id(events: &[SseEvent]) -> String {
    started(events).data["message_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A completed turn.
pub struct CompletedTurn {
    pub request_id: Uuid,
    pub message_id: String,
    pub events: Vec<SseEvent>,
}

/// Sends `question`, answers it with `answer` (usage 10/5) and asserts a completed stream.
pub async fn complete_turn(
    env: &TestEnv,
    user: &TestUser,
    chat_id: &str,
    question: &str,
    answer: &str,
) -> CompletedTurn {
    enqueue_text(env, &[answer], 10, 5);
    let events = send_ok(env, user, chat_id, question).await;
    CompletedTurn {
        request_id: stream_request_id(&events),
        message_id: stream_message_id(&events),
        events,
    }
}

// ---------------------------------------------------------------------------
// Problem assertions
// ---------------------------------------------------------------------------

/// Asserts 409 `aborted` with `context.reason == reason`.
pub fn assert_aborted(resp: &TestResponse, reason: &str) -> Value {
    let problem = assert_problem(resp, 409, Some("aborted"));
    assert_eq!(
        problem["context"]["reason"].as_str(),
        Some(reason),
        "{problem}"
    );
    problem
}

/// Asserts 400 `failed_precondition` carrying a violation `{subject, type}`.
pub fn assert_failed_precondition(
    resp: &TestResponse,
    subject: &str,
    violation_type: &str,
) -> Value {
    let problem = assert_problem(resp, 400, Some("failed_precondition"));
    let found = problem["context"]["violations"]
        .as_array()
        .is_some_and(|violations| {
            violations.iter().any(|violation| {
                violation["subject"].as_str() == Some(subject)
                    && violation["type"].as_str() == Some(violation_type)
            })
        });
    assert!(
        found,
        "expected violation {{subject: {subject}, type: {violation_type}}}: {problem}"
    );
    problem
}

/// Asserts 400 `out_of_range` with a field violation whose `reason` is `reason`.
pub fn assert_out_of_range(resp: &TestResponse, reason: &str) -> Value {
    let problem = assert_problem(resp, 400, Some("out_of_range"));
    assert!(has_violation_reason(&problem, reason), "{problem}");
    problem
}

/// Asserts 429 `resource_exhausted` whose first violation names `subject`.
pub fn assert_quota_exhausted(resp: &TestResponse, subject: &str) -> Value {
    let problem = assert_problem(resp, 429, Some("resource_exhausted"));
    assert_eq!(
        problem["context"]["violations"][0]["subject"].as_str(),
        Some(subject),
        "{problem}"
    );
    problem
}

// ---------------------------------------------------------------------------
// Direct database access (raw SQL, SQLite)
// ---------------------------------------------------------------------------

async fn query_rows(env: &TestEnv, sql: String) -> Vec<QueryResult> {
    env.db()
        .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
        .expect("row query must succeed")
}

/// Executes a statement (test fixtures only).
pub async fn exec_sql(env: &TestEnv, sql: &str) {
    env.db()
        .execute_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
        .await
        .expect("statement must succeed");
}

async fn count(env: &TestEnv, sql: String) -> i64 {
    query_rows(env, sql)
        .await
        .first()
        .expect("COUNT/SUM always returns a row")
        .try_get("", "n")
        .unwrap()
}

fn opt_uuid(row: &QueryResult, column: &str) -> Option<Uuid> {
    let hex: Option<String> = row.try_get("", column).unwrap();
    // SQLite `hex(NULL)` is the empty string, not NULL.
    hex.filter(|hex| !hex.is_empty())
        .map(|hex| Uuid::parse_str(&hex).expect("a 16-byte UUID blob"))
}

#[derive(Debug, Clone)]
pub struct TurnRow {
    pub id: Uuid,
    pub request_id: Uuid,
    pub state: String,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub provider_response_id: Option<String>,
    pub reserve_tokens: Option<i64>,
    pub max_output_tokens_applied: Option<i64>,
    pub reserved_credits_micro: Option<i64>,
    pub policy_version_applied: Option<i64>,
    pub effective_model: Option<String>,
    pub minimal_generation_floor_applied: Option<i64>,
    pub has_completed_at: bool,
    pub deleted: bool,
}

/// Every `chat_turns` row of the chat (soft-deleted ones included), oldest first.
pub async fn turn_rows(env: &TestEnv, chat_id: &str) -> Vec<TurnRow> {
    let chat = Uuid::parse_str(chat_id).expect("chat id must be a UUID");
    let sql = format!(
        "SELECT hex(id) AS id_hex, hex(request_id) AS request_hex, state, error_code, \
         hex(assistant_message_id) AS assistant_hex, provider_response_id, reserve_tokens, \
         max_output_tokens_applied, reserved_credits_micro, policy_version_applied, \
         effective_model, minimal_generation_floor_applied, \
         CASE WHEN completed_at IS NULL THEN 0 ELSE 1 END AS has_completed_at, \
         CASE WHEN deleted_at IS NULL THEN 0 ELSE 1 END AS is_deleted \
         FROM chat_turns WHERE hex(chat_id) = '{}' ORDER BY started_at, rowid",
        uuid_hex(chat)
    );
    query_rows(env, sql)
        .await
        .iter()
        .map(|row| TurnRow {
            id: opt_uuid(row, "id_hex").expect("turn id"),
            request_id: opt_uuid(row, "request_hex").expect("turn request id"),
            state: row.try_get("", "state").unwrap(),
            error_code: row.try_get("", "error_code").unwrap(),
            assistant_message_id: opt_uuid(row, "assistant_hex"),
            provider_response_id: row.try_get("", "provider_response_id").unwrap(),
            reserve_tokens: row.try_get("", "reserve_tokens").unwrap(),
            max_output_tokens_applied: row.try_get("", "max_output_tokens_applied").unwrap(),
            reserved_credits_micro: row.try_get("", "reserved_credits_micro").unwrap(),
            policy_version_applied: row.try_get("", "policy_version_applied").unwrap(),
            effective_model: row.try_get("", "effective_model").unwrap(),
            minimal_generation_floor_applied: row
                .try_get("", "minimal_generation_floor_applied")
                .unwrap(),
            has_completed_at: row.try_get::<i64>("", "has_completed_at").unwrap() != 0,
            deleted: row.try_get::<i64>("", "is_deleted").unwrap() != 0,
        })
        .collect()
}

pub async fn turn_row(env: &TestEnv, chat_id: &str, request_id: Uuid) -> Option<TurnRow> {
    turn_rows(env, chat_id)
        .await
        .into_iter()
        .find(|turn| turn.request_id == request_id)
}

/// The only turn of the chat.
pub async fn only_turn(env: &TestEnv, chat_id: &str) -> TurnRow {
    let mut turns = turn_rows(env, chat_id).await;
    assert_eq!(turns.len(), 1, "exactly one turn expected: {turns:?}");
    turns.remove(0)
}

/// Number of `chat_turns` rows of every chat.
pub async fn turn_row_count(env: &TestEnv) -> i64 {
    count(env, "SELECT COUNT(*) AS n FROM chat_turns".to_owned()).await
}

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: Uuid,
    pub request_id: Option<Uuid>,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Non-deleted `messages` rows of the chat in `(created_at, insertion)` order.
pub async fn message_rows(env: &TestEnv, chat_id: &str) -> Vec<MessageRow> {
    let chat = Uuid::parse_str(chat_id).expect("chat id must be a UUID");
    let sql = format!(
        "SELECT hex(id) AS id_hex, hex(request_id) AS request_hex, role, content, model, \
         input_tokens, output_tokens FROM messages \
         WHERE hex(chat_id) = '{}' AND deleted_at IS NULL ORDER BY created_at, rowid",
        uuid_hex(chat)
    );
    query_rows(env, sql)
        .await
        .iter()
        .map(|row| MessageRow {
            id: opt_uuid(row, "id_hex").expect("message id"),
            request_id: opt_uuid(row, "request_hex"),
            role: row.try_get("", "role").unwrap(),
            content: row.try_get("", "content").unwrap(),
            model: row.try_get("", "model").unwrap(),
            input_tokens: row.try_get("", "input_tokens").unwrap(),
            output_tokens: row.try_get("", "output_tokens").unwrap(),
        })
        .collect()
}

/// Number of `messages` rows of every chat.
pub async fn message_row_count(env: &TestEnv) -> i64 {
    count(env, "SELECT COUNT(*) AS n FROM messages".to_owned()).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaRow {
    pub period_type: String,
    pub period_start: String,
    pub bucket: String,
    pub spent: i64,
    pub reserved: i64,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// The `quota_usage` rows of `user`, ordered by period and bucket.
pub async fn quota_rows(env: &TestEnv, user: &TestUser) -> Vec<QuotaRow> {
    let sql = format!(
        "SELECT period_type, CAST(period_start AS TEXT) AS period_start, bucket, \
         spent_credits_micro, reserved_credits_micro, calls, input_tokens, output_tokens \
         FROM quota_usage WHERE hex(tenant_id) = '{}' AND hex(user_id) = '{}' \
         ORDER BY period_type, bucket",
        uuid_hex(user.tenant_id),
        uuid_hex(user.user_id)
    );
    query_rows(env, sql)
        .await
        .iter()
        .map(|row| QuotaRow {
            period_type: row.try_get("", "period_type").unwrap(),
            period_start: row.try_get("", "period_start").unwrap(),
            bucket: row.try_get("", "bucket").unwrap(),
            spent: row.try_get("", "spent_credits_micro").unwrap(),
            reserved: row.try_get("", "reserved_credits_micro").unwrap(),
            calls: row.try_get("", "calls").unwrap(),
            input_tokens: row.try_get("", "input_tokens").unwrap(),
            output_tokens: row.try_get("", "output_tokens").unwrap(),
        })
        .collect()
}

#[must_use]
pub fn quota_row<'a>(rows: &'a [QuotaRow], period: &str, bucket: &str) -> Option<&'a QuotaRow> {
    rows.iter()
        .find(|row| row.period_type == period && row.bucket == bucket)
}

/// Like `quota_row`, but the row must exist.
pub fn expect_quota_row<'a>(rows: &'a [QuotaRow], period: &str, bucket: &str) -> &'a QuotaRow {
    quota_row(rows, period, bucket)
        .unwrap_or_else(|| panic!("no quota_usage row for {period}/{bucket}: {rows:?}"))
}

/// Sum of `reserved_credits_micro` over every quota row (0 when no reserve is in flight).
pub async fn total_reserved(env: &TestEnv) -> i64 {
    count(
        env,
        "SELECT COALESCE(SUM(reserved_credits_micro), 0) AS n FROM quota_usage".to_owned(),
    )
    .await
}

/// The reaction values stored for a message.
pub async fn reaction_rows(env: &TestEnv, message_id: &str) -> Vec<String> {
    let id = Uuid::parse_str(message_id).expect("message id must be a UUID");
    let sql = format!(
        "SELECT reaction FROM message_reactions WHERE hex(message_id) = '{}'",
        uuid_hex(id)
    );
    query_rows(env, sql)
        .await
        .iter()
        .map(|row| row.try_get("", "reaction").unwrap())
        .collect()
}

/// Asserts that a rejected request left no trace: no provider chat request, no turn, no message
/// and no quota reserve.
pub async fn assert_nothing_happened(env: &TestEnv) {
    assert!(
        env.provider().chat_requests().is_empty(),
        "a rejected request must not reach the provider: {:?}",
        env.provider().chat_requests()
    );
    assert_eq!(
        turn_row_count(env).await,
        0,
        "no chat_turns row may be left"
    );
    assert_eq!(
        message_row_count(env).await,
        0,
        "no messages row may be left"
    );
    assert_eq!(total_reserved(env).await, 0, "no quota reserve may survive");
}

// ---------------------------------------------------------------------------
// Credit and token arithmetic (DESIGN 5.3 and 5.5)
// ---------------------------------------------------------------------------

#[must_use]
#[allow(
    clippy::integer_division,
    reason = "DESIGN 5.3 defines ceil_div on integers"
)]
pub fn ceil_div(numerator: i64, denominator: i64) -> i64 {
    (numerator + denominator - 1) / denominator
}

/// `credits_micro` with the per-component ceiling of DESIGN section 5.3.
#[must_use]
pub fn credits_micro(input_tokens: i64, output_tokens: i64, in_mult: i64, out_mult: i64) -> i64 {
    ceil_div(input_tokens * in_mult, 1_000_000) + ceil_div(output_tokens * out_mult, 1_000_000)
}

/// `estimated_text_tokens` of DESIGN section 5.5.4 with the `catalog_entry` budgets
/// (`bytes_per_token_conservative` 4, `fixed_overhead_tokens` 100, `safety_margin_pct` 10).
#[must_use]
pub fn estimated_text_tokens(bytes: i64) -> i64 {
    let base = ceil_div(bytes, 4) + 100;
    ceil_div(base * 110, 100)
}

/// The UTC day a daily `quota_usage` row is keyed by (now).
#[must_use]
pub fn daily_period_start() -> String {
    OffsetDateTime::now_utc().date().to_string()
}

/// The first day of the current UTC month, the key of a monthly `quota_usage` row.
#[must_use]
pub fn monthly_period_start() -> String {
    OffsetDateTime::now_utc()
        .date()
        .replace_day(1)
        .expect("day 1 exists in every month")
        .to_string()
}

/// `{tenant}/{turn}/{request}` in simple lowercase form (DESIGN section 5.6).
#[must_use]
pub fn dedupe_key(tenant: Uuid, turn: Uuid, request: Uuid) -> String {
    format!("{}/{}/{}", tenant.simple(), turn.simple(), request.simple())
}

// ---------------------------------------------------------------------------
// Messages API helpers
// ---------------------------------------------------------------------------

pub async fn list_messages(
    env: &TestEnv,
    user: &TestUser,
    chat_id: &str,
    params: &[(&str, &str)],
) -> TestResponse {
    get(env, user, &messages_url(chat_id, params)).await
}

/// Lists messages and asserts 200; returns the page.
pub async fn list_messages_ok(
    env: &TestEnv,
    user: &TestUser,
    chat_id: &str,
    params: &[(&str, &str)],
) -> Value {
    let resp = list_messages(env, user, chat_id, params).await;
    expect_status(&resp, StatusCode::OK);
    resp.json()
}

#[must_use]
pub fn message_roles(page: &Value) -> Vec<String> {
    items(page)
        .iter()
        .map(|item| item["role"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[must_use]
pub fn message_contents(page: &Value) -> Vec<String> {
    items(page)
        .iter()
        .map(|item| item["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[must_use]
pub fn message_ids(page: &Value) -> Vec<String> {
    items(page)
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Provider request inspection (what the gear sends to the provider)
// ---------------------------------------------------------------------------

/// The JSON body of the `index`-th chat request the provider received.
pub fn chat_request_body(env: &TestEnv, index: usize) -> Value {
    let requests = env.provider().chat_requests();
    let request = requests
        .get(index)
        .unwrap_or_else(|| panic!("no chat request #{index}; got {}", requests.len()));
    request
        .body_json
        .clone()
        .unwrap_or_else(|| panic!("chat request #{index} has no JSON body"))
}

/// The `input` of a Responses-API request as one string, for substring and ordering checks that do
/// not depend on whether content is a string or an array of `input_text` parts.
#[must_use]
pub fn input_text(body: &Value) -> String {
    serde_json::to_string(&body["input"]).expect("input serializes")
}

/// Position of `needle` in the serialized `input`.
pub fn input_position(body: &Value, needle: &str) -> usize {
    input_text(body)
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` is not part of the provider input: {body}"))
}

/// The system prompt as the provider got it: the `instructions` field, or a leading `system` /
/// `developer` input message.
#[must_use]
pub fn system_prompt_of(body: &Value) -> String {
    let mut text = body["instructions"].as_str().unwrap_or_default().to_owned();
    if let Some(items) = body["input"].as_array() {
        for item in items {
            if matches!(item["role"].as_str(), Some("system" | "developer")) {
                text.push_str(&serde_json::to_string(&item["content"]).unwrap_or_default());
            }
        }
    }
    text
}

// ===========================================================================
// Slice S3 (billing-ops) helpers
//
// Like the S2 helpers these use only the harness API that exists without the S3 additions of
// HARNESS.md, so every S1/S2 test binary keeps compiling. Helpers that need the S3-only harness API
// (`run_orphan_watchdog_once`, fault injection, raw outbox messages) live in `tests/billing/mod.rs`.
// ===========================================================================

pub const QUOTA_STATUS_PATH: &str = "/mini-chat/v1/quota/status";

/// The resource type of the user quota PEP resource as the 403 problem names it.
pub const USER_QUOTA_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.user_quota.v1~";

/// The system prompt guard of `context.web_search_guard` (DESIGN "Web Search Configuration").
pub const WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the \
provided context or your training data. Never use it for general knowledge questions. At most one \
web_search call per request.";

pub const XLSX_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// A timestamp literal as the tests write it into SQL (`'2026-01-01T00:00:00Z'`).
pub const SQL_TS: &str = "'2026-01-01T00:00:00Z'";

/// The 16-byte blob literal of a UUID.
#[must_use]
pub fn blob(id: Uuid) -> String {
    format!("X'{}'", id.simple())
}

/// A SQL timestamp literal in the format the gear stores (`YYYY-MM-DDTHH:MM:SSZ`).
#[must_use]
pub fn ts_literal(at: OffsetDateTime) -> String {
    format!("'{}'", at.format(&Rfc3339).expect("RFC 3339 formatting"))
}

pub async fn scalar_i64(env: &TestEnv, sql: &str) -> i64 {
    count(env, sql.to_owned()).await
}

pub async fn scalar_text(env: &TestEnv, sql: &str) -> String {
    query_rows(env, sql.to_owned())
        .await
        .first()
        .expect("the query returns a row")
        .try_get("", "t")
        .unwrap()
}

// ---------------------------------------------------------------------------
// Quota status API
// ---------------------------------------------------------------------------

pub async fn quota_status(env: &TestEnv, user: &TestUser) -> TestResponse {
    get(env, user, QUOTA_STATUS_PATH).await
}

/// `GET /v1/quota/status`, asserting 200; returns the body.
pub async fn quota_status_ok(env: &TestEnv, user: &TestUser) -> Value {
    let resp = quota_status(env, user).await;
    expect_status(&resp, StatusCode::OK);
    resp.json()
}

/// The `periods[]` entry of `tier` / `period`, when the status reports it.
#[must_use]
pub fn status_period<'a>(status: &'a Value, tier: &str, period: &str) -> Option<&'a Value> {
    status["tiers"]
        .as_array()?
        .iter()
        .find(|entry| entry["tier"] == json!(tier))?
        .get("periods")?
        .as_array()?
        .iter()
        .find(|entry| entry["period"] == json!(period))
}

/// Like `status_period`, but the entry must exist.
pub fn expect_status_period<'a>(status: &'a Value, tier: &str, period: &str) -> &'a Value {
    status_period(status, tier, period)
        .unwrap_or_else(|| panic!("no `{tier}`/`{period}` period in the quota status: {status}"))
}

/// The tier names of a status body, in order.
#[must_use]
pub fn status_tier_names(status: &Value) -> Vec<String> {
    status["tiers"]
        .as_array()
        .expect("`tiers` is an array")
        .iter()
        .map(|entry| entry["tier"].as_str().unwrap_or_default().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Quota rows
// ---------------------------------------------------------------------------

/// Counters of one seeded `quota_usage` row.
#[derive(Debug, Clone, Copy, Default)]
pub struct Seed {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

impl Seed {
    #[must_use]
    pub fn spent(spent: i64) -> Self {
        Self {
            spent,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn spent_reserved(spent: i64, reserved: i64) -> Self {
        Self {
            spent,
            reserved,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn web(web_search_calls: i64) -> Self {
        Self {
            web_search_calls,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn code(code_interpreter_calls: i64) -> Self {
        Self {
            code_interpreter_calls,
            ..Self::default()
        }
    }
}

/// Inserts (or overwrites) one `quota_usage` row of `user`.
pub async fn seed_quota_at(
    env: &TestEnv,
    user: &TestUser,
    period_type: &str,
    period_start: &str,
    bucket: &str,
    seed: Seed,
) {
    exec_sql(
        env,
        &format!(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, \
             spent_credits_micro, reserved_credits_micro, web_search_calls, code_interpreter_calls, \
             updated_at) VALUES ({}, {}, {}, '{period_type}', '{period_start}', '{bucket}', {}, {}, \
             {}, {}, {SQL_TS}) \
             ON CONFLICT (tenant_id, user_id, period_type, period_start, bucket) DO UPDATE SET \
             spent_credits_micro = excluded.spent_credits_micro, \
             reserved_credits_micro = excluded.reserved_credits_micro, \
             web_search_calls = excluded.web_search_calls, \
             code_interpreter_calls = excluded.code_interpreter_calls",
            blob(Uuid::new_v4()),
            blob(user.tenant_id),
            blob(user.user_id),
            seed.spent,
            seed.reserved,
            seed.web_search_calls,
            seed.code_interpreter_calls,
        ),
    )
    .await;
}

/// Like `seed_quota_at` for the current UTC day (`daily`) or month (`monthly`).
pub async fn seed_quota(
    env: &TestEnv,
    user: &TestUser,
    period_type: &str,
    bucket: &str,
    seed: Seed,
) {
    let start = match period_type {
        "daily" => daily_period_start(),
        "monthly" => monthly_period_start(),
        other => panic!("unknown period type `{other}`"),
    };
    seed_quota_at(env, user, period_type, &start, bucket, seed).await;
}

// ---------------------------------------------------------------------------
// Environments
// ---------------------------------------------------------------------------

/// The two-model catalog with `switches` as the policy kill switches.
pub async fn env_with_kill_switches(switches: Value) -> TestEnv {
    env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "kill_switches": switches }),
    ))
    .await
}

/// The two-model catalog with per-user limits: `total` is the overall cap, `premium` the subcap.
pub async fn env_with_user_limits(total: (i64, i64), premium: (i64, i64)) -> TestEnv {
    env_with_policy(merge(
        policy(two_model_catalog()),
        json!({
            "default_standard_limits": limits(total.0, total.1),
            "default_premium_limits": limits(premium.0, premium.1),
        }),
    ))
    .await
}

/// A premium default model and a standard model with the given `(input, output)` credit multipliers.
#[must_use]
pub fn priced_models(premium: (i64, i64), standard: (i64, i64)) -> Vec<Value> {
    vec![
        merge(
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            json!({
                "input_tokens_credit_multiplier_micro": premium.0,
                "output_tokens_credit_multiplier_micro": premium.1
            }),
        ),
        merge(
            catalog_entry(STANDARD_MODEL, "Standard", true, false),
            json!({
                "input_tokens_credit_multiplier_micro": standard.0,
                "output_tokens_credit_multiplier_micro": standard.1
            }),
        ),
    ]
}

/// A chat of `user` on `model`.
pub async fn chat_on(env: &TestEnv, user: &TestUser, model: &str) -> String {
    chat_id(&create_chat(env, user, json!({ "model": model })).await)
}

/// Points an existing chat at `model` (the catalog may have changed since the chat was created).
pub async fn set_chat_model(env: &TestEnv, chat: &str, model: &str) {
    exec_sql(
        env,
        &format!(
            "UPDATE chats SET model = '{model}' WHERE hex(id) = '{}'",
            uuid_hex(Uuid::parse_str(chat).unwrap())
        ),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Attachments (S5 owns the upload API; S3 tests need only the rows)
// ---------------------------------------------------------------------------

/// The columns of an `attachments` row a test controls.
#[derive(Debug, Clone, Copy)]
pub struct AttachmentSpec<'a> {
    pub kind: &'a str,
    pub status: &'a str,
    pub provider_file_id: &'a str,
    pub filename: &'a str,
    pub content_type: &'a str,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
}

pub async fn insert_attachment_row(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    spec: AttachmentSpec<'_>,
) -> Uuid {
    let id = Uuid::new_v4();
    exec_sql(
        env,
        &format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, \
             content_type, size_bytes, storage_backend, provider_file_id, status, attachment_kind, \
             for_file_search, for_code_interpreter, created_at, updated_at) VALUES ({}, {}, {}, {}, \
             '{}', '{}', 10, 'openai', '{}', '{}', '{}', {}, {}, {SQL_TS}, {SQL_TS})",
            blob(id),
            blob(user.tenant_id),
            blob(Uuid::parse_str(chat).unwrap()),
            blob(user.user_id),
            spec.filename,
            spec.content_type,
            spec.provider_file_id,
            spec.status,
            spec.kind,
            i32::from(spec.for_file_search),
            i32::from(spec.for_code_interpreter),
        ),
    )
    .await;
    id
}

/// A ready XLSX attachment routed to the code interpreter.
pub async fn insert_ready_xlsx(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    provider_file_id: &str,
) -> Uuid {
    insert_attachment_row(
        env,
        user,
        chat,
        AttachmentSpec {
            kind: "document",
            status: "ready",
            provider_file_id,
            filename: "data.xlsx",
            content_type: XLSX_CONTENT_TYPE,
            for_file_search: false,
            for_code_interpreter: true,
        },
    )
    .await
}

/// A ready image attachment.
pub async fn insert_ready_image(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    provider_file_id: &str,
) -> Uuid {
    insert_attachment_row(
        env,
        user,
        chat,
        AttachmentSpec {
            kind: "image",
            status: "ready",
            provider_file_id,
            filename: "cat.png",
            content_type: "image/png",
            for_file_search: false,
            for_code_interpreter: false,
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Scripted tool streams
// ---------------------------------------------------------------------------

/// A successful answer in which the provider starts `searches` web searches and completes the first
/// `completed` of them (`response.web_search_call.searching` / `.completed`).
#[must_use]
pub fn web_search_frames(searches: usize, completed: usize, usage: Option<(u64, u64)>) -> Frames {
    let mut frames = opening_frames();
    for index in 0..searches {
        frames.push(frame(
            "response.web_search_call.searching",
            json!({ "item_id": "ws_0123456789abcdef", "output_index": 0 }),
        ));
        if index < completed {
            frames.push(frame(
                "response.web_search_call.completed",
                json!({ "item_id": "ws_0123456789abcdef", "output_index": 0 }),
            ));
        }
    }
    frames.extend(delta_frames(&["answer"]));
    frames.extend(closing_frames(
        "answer",
        &json!([]),
        usage,
        "response.completed",
    ));
    frames
}

/// A successful answer in which the provider runs `calls` code interpreter calls to completion.
#[must_use]
pub fn code_interpreter_frames(calls: usize, usage: Option<(u64, u64)>) -> Frames {
    let mut frames = opening_frames();
    for index in 0..calls {
        frames.push(frame(
            "response.code_interpreter_call.in_progress",
            json!({ "item_id": "ci_0123456789abcdef", "output_index": 1 }),
        ));
        frames.push(frame(
            "response.output_item.done",
            json!({
                "output_index": 1,
                "item": {
                    "id": "ci_0123456789abcdef", "type": "code_interpreter_call",
                    "status": "completed", "container_id": "cntr_0123456789abcdef",
                    "code": format!("print({index})"),
                    "outputs": [{ "type": "logs", "logs": index.to_string() }]
                },
                "sequence_number": 8
            }),
        ));
    }
    frames.extend(delta_frames(&["answer"]));
    frames.extend(closing_frames(
        "answer",
        &json!([]),
        usage,
        "response.completed",
    ));
    frames
}

/// A stream that fails with `response.failed` after `frames` (no usage unless given).
#[must_use]
pub fn failing_after(mut frames: Frames, usage: Option<(u64, u64)>) -> ScriptedResponse {
    let mut response = json!({
        "id": PROVIDER_RESPONSE_ID,
        "object": "response",
        "status": "failed",
        "output": [],
        "error": { "code": "server_error", "message": "boom" },
    });
    if let Some((input_tokens, output_tokens)) = usage {
        response["usage"] = json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        });
    }
    frames.push(frame(
        "response.failed",
        json!({ "response": response, "sequence_number": 20 }),
    ));
    ScriptedResponse::Sse(frames)
}

// ---------------------------------------------------------------------------
// Settlement arithmetic from the persisted preflight columns
// ---------------------------------------------------------------------------

/// The credits of the estimated settlement (DESIGN 5.3.1): `credits_micro(estimated_input_tokens,
/// minimal_generation_floor_applied)` with `estimated_input_tokens = reserve_tokens -
/// max_output_tokens_applied`, all read from the turn row.
#[must_use]
pub fn estimated_credits(turn: &TurnRow, in_mult: i64, out_mult: i64) -> i64 {
    let reserve = turn.reserve_tokens.expect("reserve_tokens is persisted");
    let max_output = turn
        .max_output_tokens_applied
        .expect("max_output_tokens_applied is persisted");
    let floor = turn
        .minimal_generation_floor_applied
        .expect("minimal_generation_floor_applied is persisted");
    credits_micro(reserve - max_output, floor, in_mult, out_mult)
}

/// `credits_micro(10, 5)` at the multipliers of `catalog_entry` (the default provider usage).
#[must_use]
pub fn default_usage_credits() -> i64 {
    credits_micro(10, 5, 1_000_000, 3_000_000)
}

/// The `quota_usage` rows of `user` for the current day / month, as `(period, bucket) -> spent`
/// sums, used to compare against the quota status.
pub async fn used_of(env: &TestEnv, user: &TestUser, period: &str, bucket: &str) -> i64 {
    let rows = quota_rows(env, user).await;
    quota_row(&rows, period, bucket).map_or(0, |row| row.spent + row.reserved)
}

/// The done event of a completed stream.
pub fn done_of(events: &[SseEvent]) -> &SseEvent {
    let done = terminal(events);
    assert_eq!(
        done.event, "done",
        "expected a completed stream: {events:?}"
    );
    done
}
