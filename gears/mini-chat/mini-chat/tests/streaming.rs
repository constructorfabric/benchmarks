#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `POST /chats/{id}/messages:stream`: setup order, SSE relay, finalization,
//! replay, parallel-turn guard, cancellation (S§6.1–6.4).

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use mini_chat::infra::db::entity::{attachment, chat, chat_vector_store, quota_usage};
use mini_chat::infra::db::repos::{
    AttachmentRepo, ChatRepo, QuotaUsageRepo, TurnRepo, VectorStoreRepo,
};
use mini_chat::test_support::SseStep;
use mini_chat_sdk::{KillSwitches, TierLimits};
use sea_orm::ConnectionTrait;
use serde_json::{Value, json};
use time::{Date, Month};
use tokio::sync::Notify;
use uuid::Uuid;

use common::*;

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

/// Date of `CLOCK_START` (2025-10-09) and the first of its month.
fn today() -> Date {
    Date::from_calendar_date(2025, Month::October, 9).unwrap()
}

fn month_start() -> Date {
    Date::from_calendar_date(2025, Month::October, 1).unwrap()
}

fn usage_row(
    tenant: Uuid,
    user: Uuid,
    period: &str,
    bucket: &str,
    spent: i64,
) -> quota_usage::Model {
    let mut r = quota_row(tenant, user, bucket);
    period.clone_into(&mut r.period_type);
    r.period_start = if period == "daily" {
        today()
    } else {
        month_start()
    };
    r.spent_credits_micro = spent;
    r
}

async fn seed_quota(app: &TestApp, rows: Vec<quota_usage::Model>) {
    let conn = app.db.conn().unwrap();
    for r in rows {
        let scope = tenant_scope(r.tenant_id, r.user_id);
        QuotaUsageRepo.insert(&conn, &scope, r).await.unwrap();
    }
}

async fn chat_model(app: &TestApp, tenant: Uuid, user: Uuid, chat: Uuid) -> chat::Model {
    let conn = app.db.conn().unwrap();
    ChatRepo
        .find_by_id(&conn, &tenant_scope(tenant, user), chat)
        .await
        .unwrap()
        .unwrap()
}

/// Send and read the whole response (SSE or JSON error).
async fn send(client: &UserClient<'_>, chat: Uuid, body: &Value) -> TestResponse {
    client.post_json(&stream_path(chat), body).await
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

fn limits(std_daily: i64, prem_daily: i64) -> (TierLimits, TierLimits) {
    (
        TierLimits {
            limit_daily_credits_micro: std_daily,
            limit_monthly_credits_micro: std_daily * 30,
        },
        TierLimits {
            limit_daily_credits_micro: prem_daily,
            limit_monthly_credits_micro: prem_daily * 30,
        },
    )
}

/// Poll `f` until it returns `true` (at most `secs` seconds).
async fn eventually<F, Fut>(secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached within {secs}s");
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn messages_stream_path_is_routable() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    // Unknown chat: the route exists and reports the chat as missing.
    let resp = send(&client, Uuid::new_v4(), &json!({"content": "hi"})).await;
    let body = assert_problem(&resp, StatusCode::NOT_FOUND);
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.mini_chat.chat.v1~"
    );
}

#[tokio::test]
async fn send_streams_started_deltas_done() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    push_hello(&app);
    app.clock.advance(time::Duration::seconds(1));
    let rid = Uuid::new_v4();

    let resp = send(
        &client,
        chat,
        &json!({"content": "Hi there", "request_id": rid}),
    )
    .await;
    let events = sse_ok(&resp);

    assert_eq!(
        names(&events),
        ["stream_started", "delta", "delta", "done"],
        "{events:?}"
    );
    let started = &events[0].1;
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["is_new_turn"], true);
    let message_id: Uuid = started["message_id"].as_str().unwrap().parse().unwrap();
    assert!(started.get("thread_summary_applied").is_none(), "{started}");
    assert_eq!(events[1].1, json!({"type": "text", "content": "Hello"}));
    assert_eq!(events[2].1, json!({"type": "text", "content": " world"}));
    let done = &events[3].1;
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 10, "output_tokens": 5})
    );
    assert_eq!(done["effective_model"], "s1");
    assert_eq!(done["selected_model"], "s1");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_from").is_none(), "{done}");
    assert!(done.get("downgrade_reason").is_none(), "{done}");

    // Turn + messages.
    let turn = turn_by_request(&app, rid).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.assistant_message_id, Some(message_id));
    assert_eq!(turn.effective_model.as_deref(), Some("s1"));
    assert_eq!(turn.error_code, None);
    let msgs = all_messages(&app).await;
    assert_eq!(msgs.len(), 2);
    let user_msg = msgs.iter().find(|m| m.role == "user").unwrap();
    let asst = msgs.iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(user_msg.content, "Hi there");
    assert_eq!(asst.id, message_id);
    assert_eq!(asst.content, "Hello world");
    assert_eq!(asst.model.as_deref(), Some("s1"));
    assert_eq!((asst.input_tokens, asst.output_tokens), (10, 5));
    assert_eq!(user_msg.request_id, Some(rid));
    assert_eq!(asst.request_id, Some(rid));
    assert!(asst.created_at > user_msg.created_at);

    // chats.updated_at bumped by the send.
    let c = chat_model(&app, tenant, user, chat).await;
    assert_eq!(c.updated_at.unix_timestamp(), CLOCK_START + 1);

    // Quota settled: reserve released, spent booked, one call.
    for period in ["daily", "monthly"] {
        let row = quota_row_of(&app, period, "total").await;
        assert_eq!(row.reserved_credits_micro, 0, "{period}");
        assert!(row.spent_credits_micro > 0, "{period}");
        assert_eq!(row.calls, 1, "{period}");
        assert_eq!((row.input_tokens, row.output_tokens), (10, 5), "{period}");
    }

    // Outbox: one usage event + one audit event.
    let usage = app.outbox_payloads("mini-chat.usage_snapshot").await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["billing_outcome"], "completed");
    assert_eq!(usage[0]["settlement_method"], "actual");
    assert_eq!(usage[0]["terminal_state"], "completed");
    assert_eq!(
        usage[0]["dedupe_key"],
        format!("{}/{}/{}", tenant.simple(), turn.id.simple(), rid.simple())
    );
    assert_eq!(usage[0]["usage"]["input_tokens"], 10);
    assert!(usage[0]["actual_credits_micro"].as_i64().unwrap() > 0);
    let audit = app.outbox_payloads("mini-chat.audit").await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["event_type"], "turn_completed");
    assert_eq!(audit[0]["policy_decisions"]["quota"]["decision"], "allow");
}

#[tokio::test]
async fn server_generates_request_id_when_omitted() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    push_hello(&app);

    let events = sse_ok(&send(&client, chat, &json!({"content": "hi"})).await);
    let rid: Uuid = events[0].1["request_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(rid.get_version_num(), 4);

    let status = client.get(&turn_path(chat, rid)).await;
    assert_eq!(status.status, StatusCode::OK, "{}", status.text());
    assert_eq!(status.json()["state"], "done");
}

#[tokio::test]
async fn provider_request_contents() {
    let mut big = standard_model("big");
    big.max_output_tokens = 40_000;
    big.system_prompt = "You are a careful assistant.".to_owned();
    let app = TestApp::builder().catalog(vec![big]).build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "big").await;
    push_hello(&app);

    sse_ok(&send(&client, chat, &json!({"content": "hello"})).await);

    let reqs: Vec<_> = app
        .oagw
        .requests()
        .into_iter()
        .filter(|r| r.uri.contains("/responses"))
        .collect();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].uri, "/api.openai.com/v1/responses");
    let body = reqs[0].json_body.as_ref().unwrap();
    assert_eq!(body["model"], "big-provider-model");
    assert_eq!(body["max_output_tokens"], 32_768);
    assert_eq!(body["stream"], true);
    let user_field = body["user"].as_str().unwrap();
    assert_eq!(user_field.len(), 64);
    assert!(user_field.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(body["instructions"], "You are a careful assistant.");
    assert!(body.get("tools").is_none(), "{body}");
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.last().unwrap()["role"], "user");
    assert_eq!(input.last().unwrap()["content"], "hello");
    assert_eq!(body["metadata"]["chat_id"], chat.to_string());
    assert_eq!(body["metadata"]["request_type"], "chat");
}

// ---------------------------------------------------------------------------
// Preflight rejections (JSON, no provider call, no turn)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preflight_rejections_are_json_not_sse() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;

    let resp = send(&client, chat, &json!({"content": "   \n\t"})).await;
    let body = assert_problem(&resp, StatusCode::BAD_REQUEST);
    assert_eq!(
        field_reason(&body),
        ("content".to_owned(), "EMPTY_CONTENT".to_owned())
    );

    let a = Uuid::new_v4();
    let resp = send(
        &client,
        chat,
        &json!({"content": "hi", "attachment_ids": [a, a]}),
    )
    .await;
    let body = assert_problem(&resp, StatusCode::BAD_REQUEST);
    assert_eq!(
        field_reason(&body),
        ("attachment".to_owned(), "invalid_attachment".to_owned())
    );

    app.policy.set_kill_switches(KillSwitches {
        disable_web_search: true,
        ..KillSwitches::default()
    });
    let resp = send(
        &client,
        chat,
        &json!({"content": "hi", "web_search": {"enabled": true}}),
    )
    .await;
    let body = assert_problem(&resp, StatusCode::BAD_REQUEST);
    let v = &body["context"]["violations"][0];
    assert_eq!(v["subject"], "web_search");
    assert_eq!(v["type"], "FEATURE_DISABLED");
    app.policy.set_kill_switches(KillSwitches::default());

    // Total daily bucket used up.
    let (std_limits, _) = default_limits();
    seed_quota(
        &app,
        vec![usage_row(
            tenant,
            user,
            "daily",
            "total",
            std_limits.limit_daily_credits_micro,
        )],
    )
    .await;
    let resp = send(&client, chat, &json!({"content": "hi"})).await;
    let body = assert_problem(&resp, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(
        body["context"]["violations"][0]["description"],
        "quota_exceeded"
    );

    assert_eq!(provider_calls(&app), 0);
    assert!(all_turns(&app).await.is_empty());
    assert!(all_messages(&app).await.is_empty());
    let row = quota_row_of(&app, "daily", "total").await;
    assert_eq!(row.reserved_credits_micro, 0);
}

#[tokio::test]
async fn reserve_recheck_rejects_concurrent_booking() {
    // Preflight sees room for one reserve, but another reserve is booked in
    // the gap (simulated by a DB trigger firing on the reserve increment).
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (std_limits, _) = default_limits();
    seed_quota(
        &app,
        vec![usage_row(
            tenant,
            user,
            "daily",
            "total",
            std_limits.limit_daily_credits_micro - 10_000_000,
        )],
    )
    .await;
    app.raw
        .execute_unprepared(
            "CREATE TRIGGER concurrent_reserve AFTER UPDATE OF reserved_credits_micro \
             ON quota_usage WHEN NEW.reserved_credits_micro > OLD.reserved_credits_micro \
             AND NEW.period_type = 'daily' \
             BEGIN UPDATE quota_usage SET spent_credits_micro = spent_credits_micro + 10000000 \
             WHERE id = NEW.id; END",
        )
        .await
        .unwrap();

    let resp = send(&client, chat, &json!({"content": "hi"})).await;
    let body = assert_problem(&resp, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(provider_calls(&app), 0);
    assert!(all_turns(&app).await.is_empty());
    assert!(all_messages(&app).await.is_empty());
    let row = quota_row_of(&app, "daily", "total").await;
    assert_eq!(row.reserved_credits_micro, 0, "reserve rolled back");
}

#[tokio::test]
async fn model_removed_from_catalog_is_400_invalid_model() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "p1").await;
    app.policy.set_catalog(vec![standard_model("s1")]);

    let resp = send(&client, chat, &json!({"content": "hi"})).await;
    let body = assert_problem(&resp, StatusCode::BAD_REQUEST);
    assert_eq!(
        field_reason(&body),
        ("model".to_owned(), "INVALID_MODEL".to_owned())
    );
    assert_eq!(provider_calls(&app), 0);
    assert!(all_turns(&app).await.is_empty());
}

#[tokio::test]
async fn input_too_long_and_context_budget() {
    let mut short_input = standard_model("short");
    short_input.max_input_tokens = 10;
    let mut tiny_window = standard_model("tiny");
    tiny_window.preference = None;
    tiny_window.context_window = 4_300;
    let app = TestApp::builder()
        .catalog(vec![short_input, tiny_window])
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);

    let chat = create_chat(&client, "short").await;
    let resp = send(&client, chat, &json!({"content": "word ".repeat(200)})).await;
    let body = assert_problem(&resp, StatusCode::BAD_REQUEST);
    assert_eq!(field_reason(&body).1, "INPUT_TOO_LONG");

    let chat = create_chat(&client, "tiny").await;
    let resp = send(&client, chat, &json!({"content": "word ".repeat(200)})).await;
    let body = assert_problem(&resp, StatusCode::BAD_REQUEST);
    assert_eq!(field_reason(&body).1, "CONTEXT_BUDGET_EXCEEDED");

    assert_eq!(provider_calls(&app), 0);
    assert!(all_turns(&app).await.is_empty());
    assert!(all_messages(&app).await.is_empty());
}

// ---------------------------------------------------------------------------
// Downgrades
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_model_downgrades_with_model_disabled() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "p1").await;
    app.policy
        .set_catalog(vec![disabled(premium_model("p1")), standard_model("s1")]);
    push_hello(&app);

    let events = sse_ok(&send(&client, chat, &json!({"content": "hi"})).await);
    let done = event(&events, "done");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["effective_model"], "s1");
    assert_eq!(done["selected_model"], "p1");
    assert_eq!(done["downgrade_from"], "p1");
    assert_eq!(done["downgrade_reason"], "model_disabled");
}

#[tokio::test]
async fn premium_exhausted_downgrades() {
    let (std_l, prem_l) = limits(1_000_000_000_000, 1_000_000);
    let app = TestApp::builder().limits(std_l, prem_l).build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "p1").await;
    seed_quota(
        &app,
        vec![usage_row(tenant, user, "daily", "tier:premium", 1_000_000)],
    )
    .await;
    push_hello(&app);

    let events = sse_ok(&send(&client, chat, &json!({"content": "hi"})).await);
    let done = event(&events, "done");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["effective_model"], "s1");
    assert_ne!(done["effective_model"], done["selected_model"]);
    assert_eq!(done["downgrade_from"], "p1");
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");
    let asst = all_messages(&app)
        .await
        .into_iter()
        .find(|m| m.role == "assistant")
        .unwrap();
    assert_eq!(asst.model.as_deref(), Some("s1"));
    let audit = app.outbox_payloads("mini-chat.audit").await;
    assert_eq!(
        audit[0]["policy_decisions"]["quota"]["decision"],
        "downgrade"
    );
    assert_eq!(
        audit[0]["policy_decisions"]["quota"]["downgrade_reason"],
        "premium_quota_exhausted"
    );
}

#[tokio::test]
async fn done_quota_warnings_entries() {
    let (std_l, prem_l) = limits(1_000_000_000, 1_000_000_000);
    let app = TestApp::builder().limits(std_l, prem_l).build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    seed_quota(
        &app,
        vec![usage_row(tenant, user, "daily", "total", 850_000_000)],
    )
    .await;
    push_hello(&app);

    let events = sse_ok(&send(&client, chat, &json!({"content": "hi"})).await);
    let warnings = event(&events, "done")["quota_warnings"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    let daily_total = warnings
        .iter()
        .find(|w| w["tier"] == "total" && w["period"] == "daily")
        .unwrap();
    assert_eq!(daily_total["warning"], true);
    assert_eq!(daily_total["exhausted"], false);
    assert!(daily_total["remaining_percentage"].as_u64().unwrap() <= 15);
    assert!(daily_total["next_reset"].is_string(), "{daily_total}");
    for w in warnings.iter().filter(|w| w["warning"] == false) {
        assert!(w.get("next_reset").is_none(), "{w}");
    }
}

// ---------------------------------------------------------------------------
// Idempotency & replay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_completed_request_id() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    push_hello(&app);
    let rid = Uuid::new_v4();
    let first = sse_ok(&send(&client, chat, &json!({"content": "hi", "request_id": rid})).await);
    let persisted = first[0].1["message_id"].clone();
    let quota_before = quota_rows(&app).await;
    let usage_before = app.outbox_payloads("mini-chat.usage_snapshot").await.len();
    let audit_before = app.outbox_payloads("mini-chat.audit").await.len();

    let events = sse_ok(&send(&client, chat, &json!({"content": "hi", "request_id": rid})).await);

    assert_eq!(names(&events), ["stream_started", "delta", "done"]);
    assert_eq!(events[0].1["is_new_turn"], false);
    assert_eq!(events[0].1["request_id"], rid.to_string());
    assert_eq!(events[0].1["message_id"], persisted);
    assert_eq!(
        events[1].1,
        json!({"type": "text", "content": "Hello world"})
    );
    let done = &events[2].1;
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 10, "output_tokens": 5})
    );
    assert_eq!(done["effective_model"], "s1");
    assert_eq!(done["selected_model"], "s1");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("quota_warnings").is_none(), "{done}");
    assert_eq!(provider_calls(&app), 1);
    assert_eq!(quota_rows(&app).await, quota_before);
    assert_eq!(
        app.outbox_payloads("mini-chat.usage_snapshot").await.len(),
        usage_before
    );
    assert_eq!(
        app.outbox_payloads("mini-chat.audit").await.len(),
        audit_before
    );
    assert_eq!(all_messages(&app).await.len(), 2);
}

#[tokio::test]
async fn replay_with_downgrade_has_downgrade_from_without_reason() {
    let (std_l, prem_l) = limits(1_000_000_000_000, 1_000_000);
    let app = TestApp::builder().limits(std_l, prem_l).build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "p1").await;
    seed_quota(
        &app,
        vec![usage_row(tenant, user, "daily", "tier:premium", 1_000_000)],
    )
    .await;
    push_hello(&app);
    let rid = Uuid::new_v4();
    sse_ok(&send(&client, chat, &json!({"content": "hi", "request_id": rid})).await);

    let events = sse_ok(&send(&client, chat, &json!({"content": "hi", "request_id": rid})).await);
    let done = event(&events, "done");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["effective_model"], "s1");
    assert_eq!(done["selected_model"], "p1");
    assert_eq!(done["downgrade_from"], "p1");
    assert!(done.get("downgrade_reason").is_none(), "{done}");
}

#[tokio::test]
async fn request_id_conflict_for_failed_or_running_or_deleted() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat_id = create_chat(&client, "s1").await;
    let chat = chat_model(&app, tenant, user, chat_id).await;
    let conn = app.db.conn().unwrap();
    let scope = tenant_scope(tenant, user);
    let failed = turn_row(&chat, Uuid::new_v4(), "failed");
    let cancelled = turn_row(&chat, Uuid::new_v4(), "cancelled");
    let mut deleted = turn_row(&chat, Uuid::new_v4(), "completed");
    deleted.deleted_at = Some(ts(1_700_000_100));
    let running = turn_row(&chat, Uuid::new_v4(), "running");
    let rids: Vec<Uuid> = [&failed, &cancelled, &deleted, &running]
        .iter()
        .map(|t| t.request_id)
        .collect();
    for t in [failed, cancelled, deleted, running] {
        TurnRepo.insert(&conn, &scope, t).await.unwrap();
    }

    for rid in rids {
        let resp = send(
            &client,
            chat_id,
            &json!({"content": "hi", "request_id": rid}),
        )
        .await;
        let body = assert_problem(&resp, StatusCode::CONFLICT);
        assert_eq!(body["context"]["reason"], "request_id_conflict", "{body}");
    }
    assert_eq!(provider_calls(&app), 0);
}

/// Script a reply that sends `first` and then waits for `gate`.
fn push_held(app: &TestApp, first: &str) -> Arc<Notify> {
    let gate = Arc::new(Notify::new());
    let (d_ev, d_data) = delta(first);
    let (c_ev, c_data) = completed(3, 2);
    app.oagw.push_sse_script(
        PROVIDER_PATH,
        vec![
            SseStep::frame(d_ev, d_data),
            SseStep::Wait(Arc::clone(&gate)),
            SseStep::frame(c_ev, c_data),
        ],
    );
    gate
}

#[tokio::test]
async fn replay_checked_before_parallel_guard() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    push_hello(&app);
    let rid_a = Uuid::new_v4();
    sse_ok(&send(&client, chat, &json!({"content": "a", "request_id": rid_a})).await);

    let gate = push_held(&app, "partial");
    let mut b = client
        .open_stream(&stream_path(chat), &json!({"content": "b"}))
        .await;
    assert_eq!(b.status, StatusCode::OK);
    assert_eq!(b.next_event().await.unwrap().0, "stream_started");
    assert_eq!(b.next_event().await.unwrap().0, "delta");

    let replay = sse_ok(&send(&client, chat, &json!({"content": "a", "request_id": rid_a})).await);
    assert_eq!(names(&replay), ["stream_started", "delta", "done"]);
    assert_eq!(replay[0].1["is_new_turn"], false);

    let resp = send(
        &client,
        chat,
        &json!({"content": "c", "request_id": Uuid::new_v4()}),
    )
    .await;
    let body = assert_problem(&resp, StatusCode::CONFLICT);
    assert_eq!(body["context"]["reason"], "turn_already_running");

    gate.notify_one();
    let tail = b.rest().await;
    assert_eq!(names(&tail), ["done"]);
}

#[tokio::test]
async fn new_turn_accepted_after_terminal() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let gate = push_held(&app, "partial");
    let mut b = client
        .open_stream(&stream_path(chat), &json!({"content": "b"}))
        .await;
    assert_eq!(b.next_event().await.unwrap().0, "stream_started");

    let resp = send(&client, chat, &json!({"content": "c"})).await;
    let body = assert_problem(&resp, StatusCode::CONFLICT);
    assert_eq!(body["context"]["reason"], "turn_already_running");

    gate.notify_one();
    let tail = b.rest().await;
    assert_eq!(tail.last().unwrap().0, "done");

    push_hello(&app);
    let events = sse_ok(&send(&client, chat, &json!({"content": "c"})).await);
    assert_eq!(events.last().unwrap().0, "done");
}

#[tokio::test]
async fn insert_race_on_running_index_is_turn_already_running() {
    // Another send commits its running turn between our parallel guard and
    // our reserve transaction (simulated by a trigger that inserts a running
    // turn right before our user message).
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    app.raw
        .execute_unprepared(
            "CREATE TRIGGER racing_turn BEFORE INSERT ON messages WHEN NEW.role = 'user' \
             BEGIN INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, \
             state, started_at, updated_at) VALUES (randomblob(16), NEW.tenant_id, NEW.chat_id, \
             randomblob(16), 'user', 'running', NEW.created_at, NEW.created_at); END",
        )
        .await
        .unwrap();

    let resp = send(&client, chat, &json!({"content": "hi"})).await;
    let body = assert_problem(&resp, StatusCode::CONFLICT);
    assert_eq!(body["context"]["reason"], "turn_already_running");
    assert_eq!(provider_calls(&app), 0);
    assert!(all_turns(&app).await.is_empty());
    assert!(all_messages(&app).await.is_empty());
    assert!(quota_rows(&app).await.is_empty(), "reserve rolled back");
}

#[tokio::test]
async fn concurrent_sends_one_wins() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    for _ in 0..2 {
        app.oagw.push_sse_slow(
            PROVIDER_PATH,
            ok_reply(&["a", "b"], 3, 2),
            Duration::from_millis(150),
        );
    }

    let (b1, b2) = (json!({"content": "one"}), json!({"content": "two"}));
    let (r1, r2) = tokio::join!(send(&client, chat, &b1), send(&client, chat, &b2));

    let mut statuses = [r1.status.as_u16(), r2.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "{} / {}", r1.text(), r2.text());
    let loser = if r1.status == StatusCode::CONFLICT {
        &r1
    } else {
        &r2
    };
    assert_eq!(loser.json()["context"]["reason"], "turn_already_running");
    let winner = if r1.status == StatusCode::OK {
        &r1
    } else {
        &r2
    };
    assert_eq!(winner.sse_events().last().unwrap().0, "done");
    assert_eq!(all_turns(&app).await.len(), 1);
    assert_eq!(all_messages(&app).await.len(), 2);
    let row = quota_row_of(&app, "daily", "total").await;
    assert_eq!(row.reserved_credits_micro, 0);
}

// ---------------------------------------------------------------------------
// Provider failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_http_500_sanitized_error_event() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    app.oagw.push_json(
        PROVIDER_PATH,
        500,
        json!({"error": {"message": "Upstream failed for file-abcdefghijklmnop in resp_abcdefghijklmnop"}}),
    );
    let rid = Uuid::new_v4();

    let events = sse_ok(&send(&client, chat, &json!({"content": "hi", "request_id": rid})).await);

    assert_eq!(names(&events), ["stream_started", "error"]);
    let err = &events[1].1;
    assert_eq!(err["code"], "provider_error");
    let msg = err["message"].as_str().unwrap();
    assert!(!msg.is_empty());
    assert!(!msg.contains("file-") && !msg.contains("resp_"), "{msg}");
    let turn = turn_by_request(&app, rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert_eq!(turn.assistant_message_id, None);
    let row = quota_row_of(&app, "daily", "total").await;
    assert_eq!(row.reserved_credits_micro, 0);
    assert!(row.spent_credits_micro > 0);
    let usage = app.outbox_payloads("mini-chat.usage_snapshot").await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["billing_outcome"], "failed");
    assert_eq!(usage[0]["settlement_method"], "estimated");
    assert_eq!(usage[0]["usage"], Value::Null);
    let audit = app.outbox_payloads("mini-chat.audit").await;
    assert_eq!(audit[0]["event_type"], "turn_failed");
    assert_eq!(audit[0]["error_code"], "provider_error");

    push_hello(&app);
    let events = sse_ok(&send(&client, chat, &json!({"content": "again"})).await);
    assert_eq!(events.last().unwrap().0, "done");
}

#[tokio::test]
async fn provider_429_rate_limited() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    app.oagw.push_json_with_headers(
        PROVIDER_PATH,
        429,
        &[("retry-after", "7")],
        json!({"error": {"message": "slow down"}}),
    );
    let rid = Uuid::new_v4();

    let events = sse_ok(&send(&client, chat, &json!({"content": "hi", "request_id": rid})).await);

    assert_eq!(names(&events), ["stream_started", "error"]);
    assert_eq!(events[1].1["code"], "rate_limited");
    assert!(events[1].1["message"].as_str().unwrap().contains('7'));
    let turn = turn_by_request(&app, rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("rate_limited"));
}

#[tokio::test]
async fn provider_failed_event_settles_actual_when_usage_nonzero() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    app.oagw.push_sse(
        PROVIDER_PATH,
        vec![
            delta("par"),
            (
                "response.failed",
                json!({"type": "response.failed", "response": {
                    "id": "resp_abcdefghijklmnop",
                    "error": {"code": "server_error", "message": "model crashed"},
                    "usage": {"input_tokens": 7, "output_tokens": 3}
                }}),
            ),
        ],
    );
    let rid = Uuid::new_v4();

    let events = sse_ok(&send(&client, chat, &json!({"content": "hi", "request_id": rid})).await);

    assert_eq!(names(&events), ["stream_started", "delta", "error"]);
    assert_eq!(events[2].1["code"], "provider_error");
    assert_eq!(events[2].1["message"], "model crashed");
    let turn = turn_by_request(&app, rid).await;
    assert_eq!(turn.state, "failed");
    let usage = app.outbox_payloads("mini-chat.usage_snapshot").await;
    assert_eq!(usage[0]["billing_outcome"], "failed");
    assert_eq!(usage[0]["settlement_method"], "actual");
    assert_eq!(usage[0]["usage"]["input_tokens"], 7);
    assert_eq!(usage[0]["usage"]["output_tokens"], 3);
    let row = quota_row_of(&app, "daily", "total").await;
    assert_eq!((row.input_tokens, row.output_tokens), (7, 3));
    assert_eq!(row.reserved_credits_micro, 0);
}

#[tokio::test]
async fn web_search_calls_exceeded_mid_stream() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    app.oagw.push_sse(
        PROVIDER_PATH,
        vec![
            tool_start("web_search"),
            tool_start("web_search"),
            tool_start("web_search"),
            delta("never"),
            completed(5, 5),
        ],
    );
    let rid = Uuid::new_v4();

    let events = sse_ok(
        &send(
            &client,
            chat,
            &json!({"content": "search", "request_id": rid, "web_search": {"enabled": true}}),
        )
        .await,
    );

    assert_eq!(
        names(&events),
        ["stream_started", "tool", "tool", "error"],
        "{events:?}"
    );
    assert_eq!(
        events[1].1,
        json!({"phase": "start", "name": "web_search", "details": {}})
    );
    assert_eq!(events[3].1["code"], "web_search_calls_exceeded");
    let turn = turn_by_request(&app, rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("web_search_calls_exceeded")
    );
    assert!(turn.web_search_enabled);
    let usage = app.outbox_payloads("mini-chat.usage_snapshot").await;
    assert_eq!(usage[0]["billing_outcome"], "failed");
    assert_eq!(usage[0]["settlement_method"], "estimated");
    let body = app.oagw.requests()[0].json_body.clone().unwrap();
    assert_eq!(body["tools"][0]["type"], "web_search");
}

#[tokio::test]
async fn code_interpreter_calls_exceeded() {
    let app = TestApp::builder()
        .config(|c| c.quota.code_interpreter_max_calls_per_message = 1)
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    app.oagw.push_sse(
        PROVIDER_PATH,
        vec![
            tool_start("code_interpreter"),
            tool_start("code_interpreter"),
            completed(5, 5),
        ],
    );
    let rid = Uuid::new_v4();

    let events = sse_ok(&send(&client, chat, &json!({"content": "run", "request_id": rid})).await);

    assert_eq!(names(&events), ["stream_started", "tool", "error"]);
    assert_eq!(events[2].1["code"], "code_interpreter_calls_exceeded");
    let turn = turn_by_request(&app, rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("code_interpreter_calls_exceeded")
    );
    let usage = app.outbox_payloads("mini-chat.usage_snapshot").await;
    assert_eq!(usage[0]["settlement_method"], "estimated");
}

#[tokio::test]
async fn stream_interrupted_when_cas_lost() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let gate = push_held(&app, "partial");
    let rid = Uuid::new_v4();
    let mut live = client
        .open_stream(
            &stream_path(chat),
            &json!({"content": "hi", "request_id": rid}),
        )
        .await;
    assert_eq!(live.next_event().await.unwrap().0, "stream_started");
    assert_eq!(live.next_event().await.unwrap().0, "delta");

    // Another finalizer (the orphan watchdog) wins the CAS.
    app.raw
        .execute_unprepared(
            "UPDATE chat_turns SET state = 'failed', error_code = 'orphan_timeout' \
             WHERE state = 'running'",
        )
        .await
        .unwrap();
    gate.notify_one();

    let tail = live.rest().await;
    assert_eq!(names(&tail), ["error"], "{tail:?}");
    assert_eq!(tail[0].1["code"], "stream_interrupted");
    let turn = turn_by_request(&app, rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    assert!(all_messages(&app).await.iter().all(|m| m.role == "user"));
    assert!(
        app.outbox_payloads("mini-chat.usage_snapshot")
            .await
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Relay behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_disconnect_cancels_turn() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let gate = push_held(&app, "Partial");
    let rid = Uuid::new_v4();
    let mut live = client
        .open_stream(
            &stream_path(chat),
            &json!({"content": "hi", "request_id": rid}),
        )
        .await;
    assert_eq!(live.next_event().await.unwrap().0, "stream_started");
    let (name, data) = live.next_event().await.unwrap();
    assert_eq!(
        (name.as_str(), &data["content"]),
        ("delta", &json!("Partial"))
    );

    drop(live);

    eventually(2, || async {
        turn_by_request(&app, rid).await.state == "cancelled"
    })
    .await;
    let turn = turn_by_request(&app, rid).await;
    let asst_id = turn
        .assistant_message_id
        .expect("partial message persisted");
    let asst = all_messages(&app)
        .await
        .into_iter()
        .find(|m| m.id == asst_id)
        .unwrap();
    assert_eq!(asst.role, "assistant");
    assert_eq!(asst.content, "Partial");
    // The provider connection was dropped (the held reply never finished).
    eventually(2, || async { app.oagw.open_streams() == 0 }).await;
    drop(gate);

    let status = client.get(&turn_path(chat, rid)).await.json();
    assert_eq!(status["state"], "cancelled");
    assert_eq!(status["assistant_message_id"], asst_id.to_string());
    let usage = app.outbox_payloads("mini-chat.usage_snapshot").await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["billing_outcome"], "aborted");
    assert_eq!(usage[0]["settlement_method"], "estimated");
    assert_eq!(usage[0]["terminal_state"], "cancelled");
    let audit = app.outbox_payloads("mini-chat.audit").await;
    assert_eq!(audit[0]["event_type"], "turn_failed");
    let row = quota_row_of(&app, "daily", "total").await;
    assert_eq!(row.reserved_credits_micro, 0);

    push_hello(&app);
    let events = sse_ok(&send(&client, chat, &json!({"content": "again"})).await);
    assert_eq!(events.last().unwrap().0, "done");
}

#[tokio::test]
async fn ping_before_first_delta() {
    let app = TestApp::builder()
        .sse_ping_interval(Duration::from_millis(400))
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let (d1, d1_data) = delta("a");
    let (d2, d2_data) = delta("b");
    let (c, c_data) = completed(3, 2);
    app.oagw.push_sse_script(
        PROVIDER_PATH,
        vec![
            SseStep::Sleep(Duration::from_millis(1_100)),
            SseStep::frame(d1, d1_data),
            SseStep::Sleep(Duration::from_millis(1_100)),
            SseStep::frame(d2, d2_data),
            SseStep::frame(c, c_data),
        ],
    );

    let events = sse_ok(&send(&client, chat, &json!({"content": "think"})).await);

    let first_delta = events.iter().position(|(n, _)| n == "delta").unwrap();
    let pings: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| n == "ping")
        .map(|(i, _)| i)
        .collect();
    assert!(!pings.is_empty(), "{events:?}");
    assert!(
        pings.iter().all(|&i| i > 0 && i < first_delta),
        "{events:?}"
    );
    for i in &pings {
        assert_eq!(events[*i].1, json!({}));
    }
    assert_eq!(events.last().unwrap().0, "done");
}

#[tokio::test]
async fn deltas_relayed_before_provider_finishes() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let gate = push_held(&app, "first");

    let mut live = client
        .open_stream(&stream_path(chat), &json!({"content": "hi"}))
        .await;
    assert_eq!(live.next_event().await.unwrap().0, "stream_started");
    let (name, data) = live.next_event().await.unwrap();
    assert_eq!(name, "delta");
    assert_eq!(data["content"], "first");
    // The provider is still holding its stream open.
    assert_eq!(app.oagw.open_streams(), 1);

    gate.notify_one();
    let tail = live.rest().await;
    assert_eq!(names(&tail), ["done"]);
}

#[tokio::test]
async fn citations_once_before_done() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat_id = create_chat(&client, "s1").await;
    let chat = chat_model(&app, tenant, user, chat_id).await;
    let conn = app.db.conn().unwrap();
    let scope = tenant_scope(tenant, user);
    let doc = attachment::Model {
        filename: "Q3 Report.pdf".to_owned(),
        status: "ready".to_owned(),
        provider_file_id: Some("file-abcdefghijklmnop1234".to_owned()),
        storage_backend: "openai".to_owned(),
        ..attachment_row(&chat)
    };
    let doc_id = doc.id;
    AttachmentRepo.insert(&conn, &scope, doc).await.unwrap();
    VectorStoreRepo
        .insert(
            &conn,
            &scope,
            chat_vector_store::Model {
                vector_store_id: Some("vs_abcdefghijklmnop1234".to_owned()),
                provider: "openai".to_owned(),
                file_count: 1,
                ..vector_store_row(&chat)
            },
        )
        .await
        .unwrap();
    let annotation = |a: Value| {
        (
            "response.output_text.annotation.added",
            json!({"type": "response.output_text.annotation.added", "output_index": 0,
                   "content_index": 0, "annotation": a}),
        )
    };
    app.oagw.push_sse(
        PROVIDER_PATH,
        vec![
            delta("Answer from sources"),
            annotation(
                json!({"type": "file_citation", "file_id": "file-abcdefghijklmnop1234", "filename": "x.pdf", "index": 3}),
            ),
            annotation(
                json!({"type": "file_citation", "file_id": "file-unknownunknown9999", "filename": "y.pdf", "index": 4}),
            ),
            annotation(
                json!({"type": "url_citation", "url": "https://example.com/a", "title": "Example",
                       "start_index": 0, "end_index": 6}),
            ),
            completed(10, 5),
        ],
    );

    let events = sse_ok(&send(&client, chat_id, &json!({"content": "what?"})).await);

    assert_eq!(
        names(&events),
        ["stream_started", "delta", "citations", "done"],
        "{events:?}"
    );
    let items = events[2].1["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(
        items[0],
        json!({"source": "file", "title": "Q3 Report.pdf", "attachment_id": doc_id.to_string(), "snippet": ""})
    );
    assert_eq!(
        items[1],
        json!({"source": "web", "title": "Example", "url": "https://example.com/a",
               "snippet": "Answer", "span": {"start": 0, "end": 6}})
    );
    let text = events[2].1.to_string();
    assert!(!text.contains("file-") && !text.contains("vs_"), "{text}");
    let body = app.oagw.requests()[0].json_body.clone().unwrap();
    assert_eq!(body["tools"][0]["type"], "file_search");
    assert_eq!(
        body["tools"][0]["vector_store_ids"],
        json!(["vs_abcdefghijklmnop1234"])
    );
}

#[tokio::test]
async fn chat_list_order_reflects_last_send() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let a = create_chat(&client, "s1").await;
    app.clock.advance(time::Duration::seconds(1));
    let b = create_chat(&client, "s1").await;
    app.clock.advance(time::Duration::seconds(1));
    let first = |page: Value| page["items"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        first(client.get("/mini-chat/v1/chats").await.json()),
        b.to_string()
    );

    push_hello(&app);
    sse_ok(&send(&client, a, &json!({"content": "hi"})).await);

    assert_eq!(
        first(client.get("/mini-chat/v1/chats").await.json()),
        a.to_string()
    );
}
