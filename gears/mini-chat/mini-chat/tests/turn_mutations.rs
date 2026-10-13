#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "tests read rows unscoped through the raw connection"
)]

//! Turn mutations: retry, edit and delete of the latest turn (S§6.5,
//! D§3.9 "Turn Mutation Rules", "Summary Interaction on Turn Mutation",
//! "Audit Events for Turn Mutations").

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mini_chat::infra::db::entity::{message, message_attachment, thread_summary};
use mini_chat::infra::db::repos::{AttachmentRepo, ChatRepo, ThreadSummaryRepo, TurnRepo};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use uuid::Uuid;

use common::*;
use mini_chat::domain::clock::Clock;

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

fn retry_path(chat: Uuid, request_id: Uuid) -> String {
    format!("{}/retry", turn_path(chat, request_id))
}

async fn retry(client: &UserClient<'_>, chat: Uuid, request_id: Uuid) -> TestResponse {
    client
        .send(
            Request::builder()
                .method(Method::POST)
                .uri(retry_path(chat, request_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
}

async fn edit(
    client: &UserClient<'_>,
    chat: Uuid,
    request_id: Uuid,
    content: &str,
) -> TestResponse {
    client
        .patch_json(&turn_path(chat, request_id), &json!({"content": content}))
        .await
}

/// Send `body` as a completed turn (scripted `"Hello world"` reply); the
/// clock advances one second first so turns have distinct `started_at`.
async fn send_turn(app: &TestApp, client: &UserClient<'_>, chat: Uuid, body: Value) -> Uuid {
    app.clock.advance(time::Duration::seconds(1));
    push_hello(app);
    let resp = client.post_json(&stream_path(chat), &body).await;
    let events = sse_ok(&resp);
    assert_eq!(events.last().unwrap().0, "done", "{events:?}");
    events[0].1["request_id"].as_str().unwrap().parse().unwrap()
}

fn sse_ok(resp: &TestResponse) -> Vec<(String, Value)> {
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert!(
        resp.header("content-type")
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "{:?}",
        resp.headers
    );
    resp.sse_events()
}

fn assert_problem(resp: &TestResponse, status: StatusCode) -> Value {
    assert_eq!(resp.status, status, "{}", resp.text());
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("application/problem+json"),
        "{}",
        resp.text()
    );
    resp.json()
}

fn field_reason(body: &Value) -> (String, String) {
    let v = &body["context"]["field_violations"][0];
    (
        v["field"].as_str().unwrap_or_default().to_owned(),
        v["reason"].as_str().unwrap_or_default().to_owned(),
    )
}

fn last_provider_body(app: &TestApp) -> Value {
    app.oagw
        .requests()
        .into_iter()
        .rfind(|r| r.uri.contains("/responses"))
        .and_then(|r| r.json_body)
        .expect("a provider request")
}

fn tool_types(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .map(|x| x["type"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The audit payloads with `event_type`.
async fn audit_of(app: &TestApp, event_type: &str) -> Vec<Value> {
    app.outbox_payloads("mini-chat.audit")
        .await
        .into_iter()
        .filter(|p| p["event_type"] == event_type)
        .collect()
}

/// Non-deleted turns.
async fn live_turns(app: &TestApp) -> Vec<mini_chat::infra::db::entity::chat_turn::Model> {
    all_turns(app)
        .await
        .into_iter()
        .filter(|t| t.deleted_at.is_none())
        .collect()
}

/// Set the daily `total` bucket's spent credits (the row exists after a turn).
async fn set_daily_spent(app: &TestApp, spent: i64) {
    raw_exec(
        &app.raw,
        &format!(
            "UPDATE quota_usage SET spent_credits_micro = {spent} \
             WHERE period_type = 'daily' AND bucket = 'total'"
        ),
    )
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_latest_creates_new_turn() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let old = send_turn(&app, &client, chat, json!({"content": "Tell me a joke"})).await;

    app.clock.advance(time::Duration::seconds(5));
    push_hello(&app);
    let events = sse_ok(&retry(&client, chat, old).await);

    assert_eq!(
        events.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["stream_started", "delta", "delta", "done"],
        "{events:?}"
    );
    let started = &events[0].1;
    assert_eq!(started["is_new_turn"], true);
    let new: Uuid = started["request_id"].as_str().unwrap().parse().unwrap();
    assert_ne!(new, old);
    assert_eq!(new.get_version_num(), 4);
    let new_msg: Uuid = started["message_id"].as_str().unwrap().parse().unwrap();

    // Old turn soft-deleted and pointing to its replacement.
    let old_turn = turn_by_request(&app, old).await;
    assert!(old_turn.deleted_at.is_some());
    assert_eq!(old_turn.replaced_by_request_id, Some(new));
    let resp = client.get(&turn_path(chat, old)).await;
    let body = assert_problem(&resp, StatusCode::NOT_FOUND);
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.mini_chat.turn.v1~"
    );

    // New turn completed with filled preflight columns.
    let new_turn = turn_by_request(&app, new).await;
    assert_eq!(new_turn.state, "completed");
    assert!(new_turn.deleted_at.is_none());
    // Same message, and the replaced answer is not prior context: the
    // reserve equals the original turn's.
    assert!(new_turn.reserve_tokens.is_some());
    assert_eq!(new_turn.reserve_tokens, old_turn.reserve_tokens);
    assert_eq!(new_turn.effective_model.as_deref(), Some("s1"));
    assert_eq!(new_turn.policy_version_applied, Some(1));
    assert_eq!(new_turn.assistant_message_id, Some(new_msg));
    let status = client.get(&turn_path(chat, new)).await;
    assert_eq!(status.json()["state"], "done");

    // Messages: the copied user message + the new assistant answer.
    let msgs = client.get(&messages_path(chat)).await.json();
    let items = msgs["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{msgs}");
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[0]["content"], "Tell me a joke");
    assert_eq!(items[0]["request_id"], new.to_string());
    assert_eq!(items[1]["role"], "assistant");
    assert_eq!(items[1]["id"], new_msg.to_string());

    // The provider saw only the re-submitted message (old turn excluded).
    let provider = last_provider_body(&app);
    let input = provider["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "{provider}");
    assert_eq!(input[0]["content"], "Tell me a joke");

    // chats.updated_at bumped by the retry.
    let c = ChatRepo
        .find_by_id(&app.db.conn().unwrap(), &tenant_scope(tenant, user), chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.updated_at.unix_timestamp(), CLOCK_START + 6);

    // Usage event for the new turn; audit `turn_retry`.
    let usage = app.outbox_payloads("mini-chat.usage_snapshot").await;
    assert_eq!(usage.len(), 2, "{usage:?}");
    assert!(
        usage
            .iter()
            .any(|u| u["request_id"] == new.to_string() && u["billing_outcome"] == "completed"),
        "{usage:?}"
    );
    let audit = audit_of(&app, "turn_retry").await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["original_request_id"], old.to_string());
    assert_eq!(audit[0]["new_request_id"], new.to_string());
    assert_eq!(audit[0]["actor_user_id"], user.to_string());
    assert_eq!(audit[0]["chat_id"], chat.to_string());
    assert_eq!(audit[0]["tenant_id"], tenant.to_string());
    assert!(audit[0]["timestamp"].is_string());
}

#[tokio::test]
async fn retry_resends_web_search_flag() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let old = send_turn(
        &app,
        &client,
        chat,
        json!({"content": "news?", "web_search": {"enabled": true}}),
    )
    .await;
    assert_eq!(tool_types(&last_provider_body(&app)), ["web_search"]);

    push_hello(&app);
    let events = sse_ok(&retry(&client, chat, old).await);
    assert_eq!(events.last().unwrap().0, "done", "{events:?}");

    assert_eq!(tool_types(&last_provider_body(&app)), ["web_search"]);
    let new: Uuid = events[0].1["request_id"].as_str().unwrap().parse().unwrap();
    assert!(turn_by_request(&app, new).await.web_search_enabled);
}

#[tokio::test]
async fn old_request_id_after_retry_conflicts() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let old = send_turn(&app, &client, chat, json!({"content": "hi"})).await;
    push_hello(&app);
    sse_ok(&retry(&client, chat, old).await);

    let resp = client
        .post_json(
            &stream_path(chat),
            &json!({"content": "hi", "request_id": old}),
        )
        .await;
    let body = assert_problem(&resp, StatusCode::CONFLICT);
    assert_eq!(body["context"]["reason"], "request_id_conflict");
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_uses_new_content_and_copies_attachments() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let kept = upload_image_ready(&app, &client, chat, "file-kept").await;
    let gone = upload_image_ready(&app, &client, chat, "file-gone").await;
    let old = send_turn(
        &app,
        &client,
        chat,
        json!({"content": "what is this?", "attachment_ids": [kept, gone]}),
    )
    .await;
    // The second image is deleted after the turn (directly: referenced
    // attachments cannot be deleted through the API).
    let rows = AttachmentRepo
        .soft_delete(
            &app.db.conn().unwrap(),
            &tenant_scope(tenant, user),
            chat,
            gone,
            app.clock.now(),
        )
        .await
        .unwrap();
    assert_eq!(rows, 1);

    push_hello(&app);
    let events = sse_ok(&edit(&client, chat, old, "and now in French?").await);
    assert_eq!(events.last().unwrap().0, "done", "{events:?}");
    let new: Uuid = events[0].1["request_id"].as_str().unwrap().parse().unwrap();
    assert_ne!(new, old);

    let provider = last_provider_body(&app);
    let input = provider["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "{provider}");
    let parts = input[0]["content"].as_array().expect("multimodal parts");
    assert!(
        parts.contains(&json!({"type": "input_text", "text": "and now in French?"})),
        "{parts:?}"
    );
    assert!(
        parts.contains(&json!({"type": "input_image", "file_id": "file-kept"})),
        "{parts:?}"
    );
    assert!(
        !parts.iter().any(|p| p["file_id"] == "file-gone"),
        "{parts:?}"
    );

    // New user message: new content, only the non-deleted attachment linked.
    let new_user = all_messages(&app)
        .await
        .into_iter()
        .find(|m| m.request_id == Some(new) && m.role == "user")
        .unwrap();
    assert_eq!(new_user.content, "and now in French?");
    let links = message_attachment::Entity::find()
        .filter(message_attachment::Column::MessageId.eq(new_user.id))
        .all(&app.raw)
        .await
        .unwrap();
    assert_eq!(
        links.iter().map(|l| l.attachment_id).collect::<Vec<_>>(),
        [kept]
    );
    // The old message keeps its links (audit).
    let old_user = all_messages(&app)
        .await
        .into_iter()
        .find(|m| m.request_id == Some(old) && m.role == "user")
        .unwrap();
    assert!(old_user.deleted_at.is_some());
    let old_links = message_attachment::Entity::find()
        .filter(message_attachment::Column::MessageId.eq(old_user.id))
        .all(&app.raw)
        .await
        .unwrap();
    assert_eq!(old_links.len(), 2);

    let old_turn = turn_by_request(&app, old).await;
    assert_eq!(old_turn.replaced_by_request_id, Some(new));
    let audit = audit_of(&app, "turn_edit").await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["original_request_id"], old.to_string());
    assert_eq!(audit[0]["new_request_id"], new.to_string());
}

#[tokio::test]
async fn edit_empty_content_400() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let old = send_turn(&app, &client, chat, json!({"content": "hi"})).await;

    let body = assert_problem(
        &edit(&client, chat, old, "  \n\t ").await,
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(
        field_reason(&body),
        ("content".to_owned(), "EMPTY_CONTENT".to_owned())
    );
    assert!(turn_by_request(&app, old).await.deleted_at.is_none());
    assert_eq!(all_turns(&app).await.len(), 1);
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_latest_204_and_audit() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let first = send_turn(&app, &client, chat, json!({"content": "one"})).await;
    let second = send_turn(&app, &client, chat, json!({"content": "two"})).await;
    let updated_before = client.get(&chat_path(chat)).await.json()["updated_at"].clone();

    app.clock.advance(time::Duration::seconds(5));
    let resp = client.delete(&turn_path(chat, second)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    assert!(resp.body.is_empty());

    let t = turn_by_request(&app, second).await;
    assert!(t.deleted_at.is_some());
    assert_eq!(t.replaced_by_request_id, None);
    assert_eq!(all_turns(&app).await.len(), 2, "no new turn");
    assert_eq!(provider_calls(&app), 2);

    let msgs = client.get(&messages_path(chat)).await.json();
    let items = msgs["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{msgs}");
    assert!(items.iter().all(|m| m["request_id"] == first.to_string()));
    assert_eq!(
        client.get(&turn_path(chat, second)).await.status,
        StatusCode::NOT_FOUND
    );
    // Delete does not count as chat activity.
    assert_eq!(
        client.get(&chat_path(chat)).await.json()["updated_at"],
        updated_before
    );

    let audit = audit_of(&app, "turn_delete").await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["request_id"], second.to_string());
    assert_eq!(audit[0]["actor_user_id"], user.to_string());
    assert_eq!(audit[0]["chat_id"], chat.to_string());
    assert!(audit[0].get("new_request_id").is_none(), "{}", audit[0]);

    // The previous turn is the latest again and can be deleted too.
    let resp = client.delete(&turn_path(chat, first)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
}

// ---------------------------------------------------------------------------
// Eligibility
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_latest_is_409_not_latest_turn() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let first = send_turn(&app, &client, chat, json!({"content": "one"})).await;
    send_turn(&app, &client, chat, json!({"content": "two"})).await;

    for resp in [
        retry(&client, chat, first).await,
        edit(&client, chat, first, "x").await,
        client.delete(&turn_path(chat, first)).await,
    ] {
        let body = assert_problem(&resp, StatusCode::CONFLICT);
        assert_eq!(body["context"]["reason"], "NOT_LATEST_TURN", "{body}");
    }
    assert_eq!(live_turns(&app).await.len(), 2);
    assert_eq!(provider_calls(&app), 2);
}

#[tokio::test]
async fn already_deleted_is_409_not_latest_turn() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let only = send_turn(&app, &client, chat, json!({"content": "one"})).await;
    assert_eq!(
        client.delete(&turn_path(chat, only)).await.status,
        StatusCode::NO_CONTENT
    );

    for resp in [
        retry(&client, chat, only).await,
        edit(&client, chat, only, "x").await,
        client.delete(&turn_path(chat, only)).await,
    ] {
        let body = assert_problem(&resp, StatusCode::CONFLICT);
        assert_eq!(body["context"]["reason"], "NOT_LATEST_TURN", "{body}");
    }
    assert!(live_turns(&app).await.is_empty());
}

#[tokio::test]
async fn running_is_400_turn_state() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let (d_ev, d_data) = delta("partial");
    let (c_ev, c_data) = completed(3, 2);
    app.oagw.push_sse_script(
        PROVIDER_PATH,
        vec![
            mini_chat::test_support::SseStep::frame(d_ev, d_data),
            mini_chat::test_support::SseStep::Wait(std::sync::Arc::clone(&gate)),
            mini_chat::test_support::SseStep::frame(c_ev, c_data),
        ],
    );
    let mut live = client
        .open_stream(&stream_path(chat), &json!({"content": "slow"}))
        .await;
    let (name, started) = live.next_event().await.unwrap();
    assert_eq!(name, "stream_started");
    let running: Uuid = started["request_id"].as_str().unwrap().parse().unwrap();

    for resp in [
        retry(&client, chat, running).await,
        edit(&client, chat, running, "x").await,
        client.delete(&turn_path(chat, running)).await,
    ] {
        let body = assert_problem(&resp, StatusCode::BAD_REQUEST);
        let v = &body["context"]["violations"][0];
        assert_eq!(v["subject"], "turn_state", "{body}");
        assert_eq!(v["type"], "STATE", "{body}");
    }

    gate.notify_one();
    assert_eq!(live.rest().await.last().unwrap().0, "done");
    assert_eq!(all_turns(&app).await.len(), 1);
}

#[tokio::test]
async fn other_users_turn() {
    let app = TestApp::builder().build().await;
    let (owner, tenant) = ids();
    let client = app.as_user(owner, tenant);
    let chat = create_chat(&client, "s1").await;
    let rid = send_turn(&app, &client, chat, json!({"content": "mine"})).await;

    // Another user of the same tenant: the chat is not visible.
    let intruder = app.as_user(Uuid::new_v4(), tenant);
    for resp in [
        retry(&intruder, chat, rid).await,
        edit(&intruder, chat, rid, "x").await,
        intruder.delete(&turn_path(chat, rid)).await,
    ] {
        let body = assert_problem(&resp, StatusCode::NOT_FOUND);
        assert_eq!(
            body["context"]["resource_type"],
            "gts.cf.core.mini_chat.chat.v1~"
        );
    }

    // A turn of the owner's chat requested by someone else: 403.
    let scope = tenant_scope(tenant, owner);
    let conn = app.db.conn().unwrap();
    let chat_row = ChatRepo
        .find_by_id(&conn, &scope, chat)
        .await
        .unwrap()
        .unwrap();
    let mut foreign = turn_row(&chat_row, Uuid::new_v4(), "completed");
    foreign.requester_user_id = Some(Uuid::new_v4());
    foreign.started_at = app.clock.now() + time::Duration::seconds(10);
    let foreign_rid = foreign.request_id;
    TurnRepo.insert(&conn, &scope, foreign).await.unwrap();
    for resp in [
        retry(&client, chat, foreign_rid).await,
        edit(&client, chat, foreign_rid, "x").await,
        client.delete(&turn_path(chat, foreign_rid)).await,
    ] {
        let body = assert_problem(&resp, StatusCode::FORBIDDEN);
        assert_eq!(body["context"]["reason"], "AUTHZ_DENIED", "{body}");
    }
    assert_eq!(live_turns(&app).await.len(), 2);
}

#[tokio::test]
async fn unknown_turn_404() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    send_turn(&app, &client, chat, json!({"content": "hi"})).await;
    let unknown = Uuid::new_v4();

    for resp in [
        retry(&client, chat, unknown).await,
        edit(&client, chat, unknown, "x").await,
        client.delete(&turn_path(chat, unknown)).await,
    ] {
        let body = assert_problem(&resp, StatusCode::NOT_FOUND);
        assert_eq!(
            body["context"]["resource_type"],
            "gts.cf.core.mini_chat.turn.v1~"
        );
    }
}

#[tokio::test]
async fn pep_evaluated_once_per_mutation() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let rid = send_turn(&app, &client, chat, json!({"content": "hi"})).await;

    let before = app.pdp.requests().len();
    push_hello(&app);
    sse_ok(&retry(&client, chat, rid).await);
    let reqs = app.pdp.requests();
    assert_eq!(reqs.len(), before + 1, "{:?}", &reqs[before..]);
    assert_eq!(reqs[before].action.name, "retry_turn");
}

// ---------------------------------------------------------------------------
// Preflight and setup failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quota_rejection_leaves_previous_turn_intact() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let old = send_turn(&app, &client, chat, json!({"content": "hi"})).await;
    let (std_limits, _) = default_limits();
    set_daily_spent(&app, std_limits.limit_daily_credits_micro).await;

    for resp in [
        retry(&client, chat, old).await,
        edit(&client, chat, old, "other").await,
    ] {
        let body = assert_problem(&resp, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["context"]["violations"][0]["subject"], "tokens");
    }

    let t = turn_by_request(&app, old).await;
    assert!(t.deleted_at.is_none());
    assert_eq!(t.replaced_by_request_id, None);
    assert_eq!(all_turns(&app).await.len(), 1);
    assert_eq!(all_messages(&app).await.len(), 2);
    assert!(
        all_messages(&app)
            .await
            .iter()
            .all(|m| m.deleted_at.is_none())
    );
    assert_eq!(provider_calls(&app), 1);
    assert!(audit_of(&app, "turn_retry").await.is_empty());
}

#[tokio::test]
async fn setup_failure_after_commit_marks_new_turn_failed() {
    let mut tiny = standard_model("tiny");
    tiny.context_window = 4_600;
    let app = TestApp::builder().catalog(vec![tiny]).build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "tiny").await;
    let old = send_turn(&app, &client, chat, json!({"content": "hi"})).await;

    let resp = edit(&client, chat, old, &"word ".repeat(600)).await;
    let body = assert_problem(&resp, StatusCode::BAD_REQUEST);
    assert_eq!(field_reason(&body).1, "CONTEXT_BUDGET_EXCEEDED");

    // The mutation committed: old turn replaced, new turn failed.
    let old_turn = turn_by_request(&app, old).await;
    assert!(old_turn.deleted_at.is_some());
    let new_rid = old_turn.replaced_by_request_id.expect("replaced");
    let new_turn = turn_by_request(&app, new_rid).await;
    assert_eq!(new_turn.state, "failed");
    assert_eq!(
        new_turn.error_code.as_deref(),
        Some("context_length_exceeded")
    );
    assert!(new_turn.completed_at.is_some());
    assert_eq!(new_turn.reserve_tokens, None, "no reserve taken");
    assert_eq!(provider_calls(&app), 1);
    let status = client.get(&turn_path(chat, new_rid)).await.json();
    assert_eq!(status["state"], "error");
    assert_eq!(status["error_code"], "context_length_exceeded");

    // No settlement and no outbox events for the failed turn.
    let row = quota_row_of(&app, "daily", "total").await;
    assert_eq!(row.reserved_credits_micro, 0);
    assert_eq!(row.calls, 1);
    let usage = app.outbox_payloads("mini-chat.usage_snapshot").await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert!(usage.iter().all(|u| u["request_id"] == old.to_string()));
    assert!(audit_of(&app, "turn_failed").await.is_empty());
    assert_eq!(audit_of(&app, "turn_edit").await.len(), 1);

    // The chat is not blocked: the failed turn can be retried.
    let short = edit(&client, chat, new_rid, "short again").await;
    assert_eq!(short.status, StatusCode::OK, "{}", short.text());
}

#[tokio::test]
async fn reserve_recheck_on_retry_fails_new_turn_quota_exceeded() {
    // Preflight sees room, but another reserve is booked between the
    // preflight and the reserve (a trigger fires on the reserve increment).
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let old = send_turn(&app, &client, chat, json!({"content": "hi"})).await;
    let (std_limits, _) = default_limits();
    set_daily_spent(&app, std_limits.limit_daily_credits_micro - 10_000_000).await;
    raw_exec(
        &app.raw,
        "CREATE TRIGGER concurrent_reserve AFTER UPDATE OF reserved_credits_micro \
         ON quota_usage WHEN NEW.reserved_credits_micro > OLD.reserved_credits_micro \
         AND NEW.period_type = 'daily' \
         BEGIN UPDATE quota_usage SET spent_credits_micro = spent_credits_micro + 10000000 \
         WHERE id = NEW.id; END",
    )
    .await
    .unwrap();

    let resp = retry(&client, chat, old).await;
    let body = assert_problem(&resp, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(
        body["context"]["violations"][0]["description"],
        "quota_exceeded"
    );

    let old_turn = turn_by_request(&app, old).await;
    assert!(
        old_turn.deleted_at.is_some(),
        "previous turn already replaced"
    );
    let new_turn = turn_by_request(&app, old_turn.replaced_by_request_id.unwrap()).await;
    assert_eq!(new_turn.state, "failed");
    assert_eq!(new_turn.error_code.as_deref(), Some("quota_exceeded"));
    assert_eq!(new_turn.reserve_tokens, None, "reserve rolled back");
    let row = quota_row_of(&app, "daily", "total").await;
    assert_eq!(row.reserved_credits_micro, 0);
    assert_eq!(provider_calls(&app), 1);
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_retries_deterministic() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let old = send_turn(&app, &client, chat, json!({"content": "hi"})).await;
    for _ in 0..2 {
        app.oagw.push_sse_slow(
            PROVIDER_PATH,
            ok_reply(&["a", "b"], 3, 2),
            Duration::from_millis(150),
        );
    }

    let (r1, r2) = tokio::join!(retry(&client, chat, old), retry(&client, chat, old));

    let mut statuses = [r1.status.as_u16(), r2.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "{} / {}", r1.text(), r2.text());
    let (winner, loser) = if r1.status == StatusCode::OK {
        (&r1, &r2)
    } else {
        (&r2, &r1)
    };
    let reason = loser.json()["context"]["reason"].clone();
    assert!(
        reason == "GENERATION_IN_PROGRESS" || reason == "NOT_LATEST_TURN",
        "{reason}"
    );
    assert_eq!(winner.sse_events().last().unwrap().0, "done");

    let live = live_turns(&app).await;
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0].state, "completed");
    assert_eq!(all_turns(&app).await.len(), 2);
    assert_eq!(audit_of(&app, "turn_retry").await.len(), 1);
}

// ---------------------------------------------------------------------------
// Thread summary interaction
// ---------------------------------------------------------------------------

async fn summaries(app: &TestApp) -> Vec<thread_summary::Model> {
    thread_summary::Entity::find().all(&app.raw).await.unwrap()
}

/// Insert a summary of `chat` whose frontier is the message `upto`.
async fn insert_summary(app: &TestApp, tenant: Uuid, user: Uuid, chat: Uuid, upto: Uuid) {
    let scope = tenant_scope(tenant, user);
    let conn = app.db.conn().unwrap();
    let chat_row = ChatRepo
        .find_by_id(&conn, &scope, chat)
        .await
        .unwrap()
        .unwrap();
    let msg = all_messages(app)
        .await
        .into_iter()
        .find(|m| m.id == upto)
        .unwrap();
    ThreadSummaryRepo
        .insert(&conn, &scope, thread_summary_row(&chat_row, &msg))
        .await
        .unwrap();
    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(message::Column::ChatId.eq(chat))
        .filter(message::Column::DeletedAt.is_null())
        .exec(&app.raw)
        .await
        .unwrap();
}

async fn compressed_count(app: &TestApp, chat: Uuid) -> usize {
    all_messages(app)
        .await
        .iter()
        .filter(|m| m.chat_id == chat && m.is_compressed)
        .count()
}

async fn user_message_of(app: &TestApp, request_id: Uuid) -> Uuid {
    all_messages(app)
        .await
        .into_iter()
        .find(|m| m.request_id == Some(request_id) && m.role == "user")
        .unwrap()
        .id
}

#[tokio::test]
async fn mutation_deletes_covering_summary() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);

    // Delete: the summary covers the latest turn's user message.
    let chat = create_chat(&client, "s1").await;
    send_turn(&app, &client, chat, json!({"content": "one"})).await;
    let second = send_turn(&app, &client, chat, json!({"content": "two"})).await;
    insert_summary(
        &app,
        tenant,
        user,
        chat,
        user_message_of(&app, second).await,
    )
    .await;
    assert!(compressed_count(&app, chat).await > 0);
    assert_eq!(
        client.delete(&turn_path(chat, second)).await.status,
        StatusCode::NO_CONTENT
    );
    assert!(summaries(&app).await.iter().all(|s| s.chat_id != chat));
    assert_eq!(compressed_count(&app, chat).await, 0);

    // Retry: same rule.
    let chat2 = create_chat(&client, "s1").await;
    let only = send_turn(&app, &client, chat2, json!({"content": "solo"})).await;
    insert_summary(&app, tenant, user, chat2, user_message_of(&app, only).await).await;
    push_hello(&app);
    sse_ok(&retry(&client, chat2, only).await);
    assert!(summaries(&app).await.iter().all(|s| s.chat_id != chat2));
    assert_eq!(compressed_count(&app, chat2).await, 0);

    // A summary that ends before the mutated turn is kept.
    let chat3 = create_chat(&client, "s1").await;
    let first3 = send_turn(&app, &client, chat3, json!({"content": "a"})).await;
    let second3 = send_turn(&app, &client, chat3, json!({"content": "b"})).await;
    let first_assistant = all_messages(&app)
        .await
        .into_iter()
        .find(|m| m.request_id == Some(first3) && m.role == "assistant")
        .unwrap()
        .id;
    insert_summary(&app, tenant, user, chat3, first_assistant).await;
    let compressed = compressed_count(&app, chat3).await;
    assert_eq!(
        client.delete(&turn_path(chat3, second3)).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        summaries(&app)
            .await
            .iter()
            .filter(|s| s.chat_id == chat3)
            .count(),
        1
    );
    assert!(compressed_count(&app, chat3).await > 0);
    assert!(compressed_count(&app, chat3).await <= compressed);
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn routes_match_openapi_operation_ids() {
    let app = TestApp::builder().build().await;
    let ops: Vec<(String, String)> = app
        .operations()
        .into_iter()
        .filter(|o| o.path.contains("/turns/"))
        .map(|o| (o.method.to_string(), o.operation_id.unwrap_or_default()))
        .collect();
    for expected in [
        ("GET", "mini_chat.get_turn"),
        ("POST", "mini_chat.retry_turn"),
        ("PATCH", "mini_chat.edit_turn"),
        ("DELETE", "mini_chat.delete_turn"),
    ] {
        assert!(
            ops.iter()
                .any(|(m, id)| m == expected.0 && id == expected.1),
            "{expected:?} not in {ops:?}"
        );
    }
}
