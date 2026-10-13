//! Helpers for the streaming tests: chat creation, scripted provider
//! replies (`OpenAI` Responses SSE) and unscoped DB reads.

#![allow(
    clippy::disallowed_methods,
    reason = "tests read rows unscoped through the raw connection"
)]

use axum::http::StatusCode;
use sea_orm::{EntityTrait, QueryOrder};
use serde_json::{Value, json};
use uuid::Uuid;

use mini_chat::infra::db::entity::{chat_turn, message, quota_usage};

use super::app::{TestApp, UserClient};

/// Provider path of the default `openai` entry (`FakeOagw` prefix).
pub const PROVIDER_PATH: &str = "/v1/responses";

pub fn stream_path(chat: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/messages:stream")
}

pub fn messages_path(chat: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/messages")
}

pub fn turn_path(chat: Uuid, request_id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/turns/{request_id}")
}

pub fn chat_path(chat: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}")
}

/// Create a chat with `model`; returns its id.
pub async fn create_chat(client: &UserClient<'_>, model: &str) -> Uuid {
    let resp = client
        .post_json("/mini-chat/v1/chats", &json!({"model": model}))
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    resp.json()["id"].as_str().unwrap().parse().unwrap()
}

/// `response.output_text.delta` frame.
pub fn delta(text: &str) -> (&'static str, Value) {
    (
        "response.output_text.delta",
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": text}),
    )
}

/// `response.completed` frame with usage.
pub fn completed(input: i64, output: i64) -> (&'static str, Value) {
    (
        "response.completed",
        json!({"type": "response.completed", "response": {
            "id": "resp_abcdefghijklmnop1234",
            "usage": {"input_tokens": input, "output_tokens": output}
        }}),
    )
}

/// A built-in tool start frame (`web_search` / `file_search` /
/// `code_interpreter`).
pub fn tool_start(tool: &str) -> (&'static str, Value) {
    let name = match tool {
        "web_search" => "response.web_search_call.searching",
        "file_search" => "response.file_search_call.searching",
        "code_interpreter" => "response.code_interpreter_call.in_progress",
        other => panic!("unknown tool {other}"),
    };
    (name, json!({"type": name}))
}

/// Deltas of `texts` followed by `response.completed(input, output)`.
pub fn ok_reply(texts: &[&str], input: i64, output: i64) -> Vec<(&'static str, Value)> {
    let mut out: Vec<_> = texts.iter().map(|t| delta(t)).collect();
    out.push(completed(input, output));
    out
}

/// Script a successful reply `"Hello world"` (usage 10 / 5).
pub fn push_hello(app: &TestApp) {
    app.oagw
        .push_sse(PROVIDER_PATH, ok_reply(&["Hello", " world"], 10, 5));
}

/// Number of provider (`/responses`) requests recorded by the fake.
pub fn provider_calls(app: &TestApp) -> usize {
    app.oagw
        .requests()
        .iter()
        .filter(|r| r.uri.contains("/responses"))
        .count()
}

/// Every turn row (unscoped).
pub async fn all_turns(app: &TestApp) -> Vec<chat_turn::Model> {
    chat_turn::Entity::find()
        .order_by_asc(chat_turn::Column::StartedAt)
        .all(&app.raw)
        .await
        .unwrap()
}

/// The turn with `request_id`.
pub async fn turn_by_request(app: &TestApp, request_id: Uuid) -> chat_turn::Model {
    all_turns(app)
        .await
        .into_iter()
        .find(|t| t.request_id == request_id)
        .unwrap_or_else(|| panic!("no turn with request_id {request_id}"))
}

/// Every message row (unscoped), oldest first.
pub async fn all_messages(app: &TestApp) -> Vec<message::Model> {
    message::Entity::find()
        .order_by_asc(message::Column::CreatedAt)
        .all(&app.raw)
        .await
        .unwrap()
}

/// Every `quota_usage` row (unscoped).
pub async fn quota_rows(app: &TestApp) -> Vec<quota_usage::Model> {
    quota_usage::Entity::find().all(&app.raw).await.unwrap()
}

/// The `quota_usage` row of `(period_type, bucket)` (single user tests).
pub async fn quota_row_of(app: &TestApp, period: &str, bucket: &str) -> quota_usage::Model {
    quota_rows(app)
        .await
        .into_iter()
        .find(|r| r.period_type == period && r.bucket == bucket)
        .unwrap_or_else(|| panic!("no quota row {period}/{bucket}"))
}

/// Event names of an SSE event list.
pub fn names(events: &[(String, Value)]) -> Vec<&str> {
    events.iter().map(|(n, _)| n.as_str()).collect()
}

/// Data of the first event named `name`.
pub fn event<'a>(events: &'a [(String, Value)], name: &str) -> &'a Value {
    &events
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no `{name}` event in {events:?}"))
        .1
}
