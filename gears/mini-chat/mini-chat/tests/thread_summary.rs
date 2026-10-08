#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "tests read rows unscoped through the raw connection"
)]

//! Thread summary (S§10.2, D§3.6 "Thread Summary Update"): trigger in the
//! finalization of completed turns, the outbox task (summary call, CAS
//! commit, system usage event) and the summary in the next turn's context.

mod common;

use axum::http::StatusCode;
use mini_chat::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use mini_chat::domain::clock::Clock;
use mini_chat::domain::context::SUMMARY_PREAMBLE;
use mini_chat::domain::ports::{HandlerOutcome, ThreadSummaryRunner};
use mini_chat::infra::db::entity::{message, thread_summary};
use mini_chat::infra::db::repos::{ChatRepo, ThreadSummaryRepo};
use mini_chat::infra::outbox::payloads::ThreadSummaryPayload;
use mini_chat_sdk::ModelCatalogEntry;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use uuid::Uuid;

use common::*;

const TINY: &str = "tiny";
const SUMMARY_MODEL: &str = "gpt-4.1-mini";
const THREAD_SUMMARY_QUEUE: &str = "mini-chat.thread_summary";
const USAGE_QUEUE: &str = "mini-chat.usage_snapshot";
/// Platform default subject: the identity of the summary call.
const SYSTEM_SUBJECT: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);

/// Like `gpt-4.1-mini-tiny-ctx`: 4096-token window, so ~8 KB of content
/// crosses the 80 % threshold of the 3072-token input budget.
fn tiny_model() -> ModelCatalogEntry {
    let mut e = standard_model(TINY);
    e.context_window = 4096;
    e.max_output_tokens = 1024;
    e.max_input_tokens = 3072;
    e
}

fn summary_model() -> ModelCatalogEntry {
    standard_no_vision(SUMMARY_MODEL)
}

async fn app_with(catalog: Vec<ModelCatalogEntry>) -> TestApp {
    TestApp::builder().catalog(catalog).build().await
}

async fn app() -> TestApp {
    app_with(vec![tiny_model(), summary_model()]).await
}

/// Content big enough to cross the proactive threshold on its own turn.
fn big(tag: &str) -> String {
    format!("{tag} {}", "x".repeat(8000))
}

/// Send `content` as a completed turn answered with `answer`; the clock
/// advances one second first. Returns `(request_id, stream_started data)`.
async fn send(
    app: &TestApp,
    client: &UserClient<'_>,
    chat: Uuid,
    content: &str,
    answer: &str,
) -> (Uuid, Value) {
    app.clock.advance(time::Duration::seconds(1));
    app.oagw
        .push_sse(PROVIDER_PATH, ok_reply(&[answer], 100, 10));
    let resp = client
        .post_json(&stream_path(chat), &json!({"content": content}))
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let events = resp.sse_events();
    assert_eq!(events.last().unwrap().0, "done", "{events:?}");
    let started = event(&events, "stream_started").clone();
    let rid = started["request_id"].as_str().unwrap().parse().unwrap();
    (rid, started)
}

async fn summary_payloads(app: &TestApp) -> Vec<ThreadSummaryPayload> {
    app.outbox_payloads(THREAD_SUMMARY_QUEUE)
        .await
        .into_iter()
        .map(|v| serde_json::from_value(v).expect("thread summary payload"))
        .collect()
}

async fn system_usage_events(app: &TestApp) -> Vec<Value> {
    app.outbox_payloads(USAGE_QUEUE)
        .await
        .into_iter()
        .filter(|p| p["requester_type"] == "system")
        .collect()
}

async fn summary_row(app: &TestApp, chat: Uuid) -> Option<thread_summary::Model> {
    thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat))
        .one(&app.raw)
        .await
        .unwrap()
}

/// The non-deleted message of turn `rid` with `role`.
async fn turn_message(app: &TestApp, rid: Uuid, role: &str) -> message::Model {
    all_messages(app)
        .await
        .into_iter()
        .find(|m| m.request_id == Some(rid) && m.role == role && m.deleted_at.is_none())
        .unwrap()
}

async fn compressed_ids(app: &TestApp, chat: Uuid) -> Vec<Uuid> {
    all_messages(app)
        .await
        .into_iter()
        .filter(|m| m.chat_id == chat && m.is_compressed)
        .map(|m| m.id)
        .collect()
}

/// Insert a summary `"summary"` up to message `upto` (optionally marking
/// every message up to it compressed).
async fn insert_summary(
    app: &TestApp,
    client: &UserClient<'_>,
    chat: Uuid,
    upto: &message::Model,
    compress: bool,
) {
    let scope = tenant_scope(client.ctx.subject_tenant_id(), client.ctx.subject_id());
    let conn = app.db.conn().unwrap();
    let chat_row = ChatRepo
        .find_by_id(&conn, &scope, chat)
        .await
        .unwrap()
        .unwrap();
    ThreadSummaryRepo
        .insert(&conn, &scope, thread_summary_row(&chat_row, upto))
        .await
        .unwrap();
    if compress {
        message::Entity::update_many()
            .col_expr(message::Column::IsCompressed, Expr::value(true))
            .filter(message::Column::ChatId.eq(chat))
            .filter(message::Column::CreatedAt.lte(upto.created_at))
            .exec(&app.raw)
            .await
            .unwrap();
    }
}

/// `OpenAI` Responses non-streaming body with `text` and usage.
fn summary_response(text: &str, output_tokens: i64, reasoning_tokens: i64) -> Value {
    json!({
        "id": "resp_summary000000000001",
        "status": "completed",
        "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]}],
        "usage": {"input_tokens": 500, "output_tokens": output_tokens,
                  "output_tokens_details": {"reasoning_tokens": reasoning_tokens}}
    })
}

fn last_provider_body(app: &TestApp) -> Value {
    app.oagw
        .requests()
        .into_iter()
        .rfind(|r| r.uri.contains("/responses"))
        .and_then(|r| r.json_body)
        .expect("a provider request")
}

/// Two turns (`first question` → `first answer`, then a big one) leave one
/// queued summary task whose target is the first answer.
struct Triggered {
    chat: Uuid,
    first: Uuid,
    second: Uuid,
    payload: ThreadSummaryPayload,
}

async fn triggered(app: &TestApp, client: &UserClient<'_>) -> Triggered {
    let chat = create_chat(client, TINY).await;
    let (first, _) = send(app, client, chat, "first question", "first answer").await;
    let (second, _) = send(app, client, chat, &big("second question"), "second answer").await;
    let mut payloads = summary_payloads(app).await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    Triggered {
        chat,
        first,
        second,
        payload: payloads.remove(0),
    }
}

/// [`triggered`] plus a committed summary `"S"` (token estimate 20).
async fn summarized(app: &TestApp, client: &UserClient<'_>) -> Triggered {
    let t = triggered(app, client).await;
    app.oagw.push_json(
        PROVIDER_PATH,
        200,
        summary_response("<analysis>thinking</analysis>\n<summary>S</summary>", 20, 0),
    );
    let outcome = app.services.summaries.run(t.payload.clone(), 1).await;
    assert_eq!(outcome, HandlerOutcome::Ok);
    t
}

#[tokio::test]
async fn proactive_trigger_enqueues_with_frozen_target() {
    let app = app().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let client = app.as_user(user, tenant);

    // A first turn, even over the threshold, has no earlier message to summarize.
    let lone = create_chat(&client, TINY).await;
    send(&app, &client, lone, &big("lone"), "answer").await;
    assert!(summary_payloads(&app).await.is_empty());

    let t = triggered(&app, &client).await;
    let target = turn_message(&app, t.first, "assistant").await;
    let p = &t.payload;
    assert_eq!(p.tenant_id, tenant);
    assert_eq!(p.chat_id, t.chat);
    assert_eq!(p.base_frontier(), None);
    // The causing turn is never summarized: the target is the message before it.
    assert_eq!(p.frozen_target(), (target.created_at, target.id));
    assert_eq!(p.system_task_type, "thread_summary_update");
    assert_eq!(p.system_request_id.get_version_num(), 4);
    let raw = &app.outbox_payloads(THREAD_SUMMARY_QUEUE).await[0];
    assert!(raw["base_frontier_message_id"].is_null(), "{raw}");
}

#[tokio::test]
async fn handler_commits_summary_and_marks_compressed() {
    let app = app().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let client = app.as_user(user, tenant);
    let t = summarized(&app, &client).await;

    let target = turn_message(&app, t.first, "assistant").await;
    let first_user = turn_message(&app, t.first, "user").await;
    let row = summary_row(&app, t.chat).await.expect("summary committed");
    assert_eq!(row.summary_text, "S");
    assert_eq!(row.tenant_id, tenant);
    assert_eq!(
        (
            row.summarized_up_to_created_at,
            row.summarized_up_to_message_id
        ),
        (target.created_at, target.id)
    );
    assert_eq!(row.token_estimate, 20);

    let mut compressed = compressed_ids(&app, t.chat).await;
    compressed.sort();
    let mut expected = vec![first_user.id, target.id];
    expected.sort();
    assert_eq!(
        compressed, expected,
        "exactly the frozen range is compressed"
    );

    let usage = system_usage_events(&app).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    let ev = &usage[0];
    assert_eq!(ev["billing_outcome"], "system_task");
    assert_eq!(ev["settlement_method"], "none");
    assert_eq!(ev["actual_credits_micro"], 0);
    assert_eq!(ev["terminal_state"], "completed");
    assert_eq!(ev["tenant_id"], tenant.to_string());
    assert_eq!(ev["chat_id"], t.chat.to_string());
    assert_eq!(ev["request_id"], t.payload.system_request_id.to_string());
    assert_eq!(ev["effective_model"], SUMMARY_MODEL);
    assert_eq!(ev["selected_model"], SUMMARY_MODEL);
    assert_eq!(ev["system_task_type"], "thread_summary_update");
    assert_eq!(
        ev["dedupe_key"],
        format!(
            "{}/thread_summary_update/{}",
            tenant.simple(),
            t.payload.system_request_id.simple()
        )
    );
    assert_eq!(ev["usage"]["output_tokens"], 20);
    let obj = ev.as_object().unwrap();
    assert!(!obj.contains_key("user_id"), "{ev}");
    assert!(!obj.contains_key("turn_id"), "{ev}");

    let body = last_provider_body(&app);
    assert_eq!(body["stream"], false, "{body}");
    assert_eq!(body["metadata"]["request_type"], "summary");
    assert_eq!(body["metadata"]["feature"], "none");
    assert_eq!(body["metadata"]["user_id"], SYSTEM_SUBJECT.to_string());
    assert_eq!(body["metadata"]["chat_id"], t.chat.to_string());
    assert_eq!(
        body["user"],
        format!("{}{}", tenant.simple(), SYSTEM_SUBJECT.simple())
    );
    assert_eq!(body["model"], format!("{SUMMARY_MODEL}-provider-model"));
    // The summary model's catalog max_output_tokens.
    assert_eq!(body["max_output_tokens"], 4096);
    assert_eq!(body["instructions"], DEFAULT_SUMMARY_SYSTEM_PROMPT);
    assert!(body.get("tools").is_none(), "{body}");
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "{body}");
    assert_eq!(input[0]["role"], "user");
    let prompt = input[0]["content"].as_str().unwrap();
    assert!(
        prompt.starts_with(
            "Summarize the following conversation:\n\nUser: first question\n\nAssistant: first answer\n\n"
        ),
        "{prompt}"
    );
    assert!(!prompt.contains("second question"), "causing turn excluded");
}

#[tokio::test]
async fn next_turn_uses_summary() {
    let app = app().await;
    let client = app.as_user(Uuid::new_v4(), Uuid::new_v4());
    let t = summarized(&app, &client).await;

    let (_, started) = send(&app, &client, t.chat, "third question", "third answer").await;
    assert_eq!(
        started["thread_summary_applied"]["token_estimate"], 20,
        "{started}"
    );
    let body = last_provider_body(&app);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "user");
    let first = input[0]["content"].as_str().unwrap();
    assert!(first.starts_with(SUMMARY_PREAMBLE), "{first}");
    assert!(first.ends_with("\n\nS"), "{first}");
    let contents: Vec<&str> = input
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(!contents.contains(&"first question"), "{contents:?}");
    assert!(!contents.contains(&"first answer"), "{contents:?}");
    // Messages after the frontier follow, then the current message.
    assert!(
        contents.iter().any(|c| c.starts_with("second question")),
        "{contents:?}"
    );
    assert!(contents.contains(&"second answer"), "{contents:?}");
    assert_eq!(*contents.last().unwrap(), "third question");
    let _ = t.second;
}

#[tokio::test]
async fn recent_history_starts_after_frontier() {
    let app = app().await;
    let client = app.as_user(Uuid::new_v4(), Uuid::new_v4());
    let chat = create_chat(&client, TINY).await;
    let (first, _) = send(&app, &client, chat, "first question", "first answer").await;
    // Summary row without compressed flags: the frontier alone excludes them.
    let upto = turn_message(&app, first, "assistant").await;
    insert_summary(&app, &client, chat, &upto, false).await;

    let (_, started) = send(&app, &client, chat, "second question", "second answer").await;
    assert_eq!(
        started["thread_summary_applied"]["token_estimate"], 3,
        "{started}"
    );
    let body = last_provider_body(&app);
    let contents: Vec<&str> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents.len(), 2, "{contents:?}");
    assert!(contents[0].starts_with(SUMMARY_PREAMBLE));
    assert_eq!(contents[1], "second question");
}

#[tokio::test]
async fn provider_failure_keeps_previous_summary_retry_then_reject() {
    let app = app().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let client = app.as_user(user, tenant);
    let t = summarized(&app, &client).await;
    let before = summary_row(&app, t.chat).await.unwrap();
    let compressed_before = compressed_ids(&app, t.chat).await;

    let (third, _) = send(&app, &client, t.chat, "third question", "third answer").await;
    let target = turn_message(&app, t.second, "assistant").await;
    let payload = ThreadSummaryPayload::new(
        tenant,
        t.chat,
        Some((
            before.summarized_up_to_created_at,
            before.summarized_up_to_message_id,
        )),
        (target.created_at, target.id),
    );
    let max = app.config.thread_summary_worker.max_attempts;
    assert_eq!(max, 3);
    let calls_before = provider_calls(&app);
    for attempt in 1..=max {
        app.oagw.push_json(
            PROVIDER_PATH,
            500,
            json!({"error": {"message": "upstream exploded", "type": "server_error"}}),
        );
        let outcome = app.services.summaries.run(payload.clone(), attempt).await;
        if attempt < max {
            assert_eq!(outcome, HandlerOutcome::Retry, "attempt {attempt}");
        } else {
            assert!(
                matches!(&outcome, HandlerOutcome::Reject(r) if r.contains("provider_error")),
                "{outcome:?}"
            );
        }
        assert_eq!(summary_row(&app, t.chat).await.unwrap(), before);
    }
    assert_eq!(provider_calls(&app), calls_before + max as usize);
    // The existing summary was part of the merge prompt.
    let prompt = last_provider_body(&app)["input"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        prompt.contains("<existing_summary>\nS\n</existing_summary>"),
        "{prompt}"
    );
    assert!(prompt.contains("User: second question"), "{prompt}");
    assert!(
        !prompt.contains("third question"),
        "beyond the frozen target"
    );
    assert_eq!(compressed_ids(&app, t.chat).await, compressed_before);
    assert_eq!(
        system_usage_events(&app).await.len(),
        1,
        "only the first commit"
    );
    let _ = third;
}

#[tokio::test]
async fn cas_conflict_is_ok() {
    let app = app().await;
    let client = app.as_user(Uuid::new_v4(), Uuid::new_v4());
    let t = summarized(&app, &client).await;
    let before = summary_row(&app, t.chat).await.unwrap();
    let calls = provider_calls(&app);

    // Redelivery of the committed task: the frontier is no longer its base.
    let outcome = app.services.summaries.run(t.payload.clone(), 2).await;
    assert_eq!(outcome, HandlerOutcome::Ok);
    assert_eq!(summary_row(&app, t.chat).await.unwrap(), before);
    assert_eq!(provider_calls(&app), calls, "lost at the pre-check");
    assert_eq!(system_usage_events(&app).await.len(), 1);
}

#[tokio::test]
async fn frontier_deleted_skips_commit() {
    let app = app().await;
    let client = app.as_user(Uuid::new_v4(), Uuid::new_v4());
    let t = triggered(&app, &client).await;
    let target = turn_message(&app, t.first, "assistant").await;
    message::Entity::update_many()
        .col_expr(message::Column::DeletedAt, Expr::value(app.clock.now()))
        .filter(message::Column::Id.eq(target.id))
        .exec(&app.raw)
        .await
        .unwrap();

    app.oagw.push_json(
        PROVIDER_PATH,
        200,
        summary_response("<summary>S</summary>", 20, 0),
    );
    let outcome = app.services.summaries.run(t.payload.clone(), 1).await;
    assert_eq!(outcome, HandlerOutcome::Ok);
    assert!(summary_row(&app, t.chat).await.is_none());
    assert!(compressed_ids(&app, t.chat).await.is_empty());
    assert!(system_usage_events(&app).await.is_empty());
}

#[tokio::test]
async fn missing_summary_model_rejects() {
    // The trigger does not check the summary model: work is still enqueued.
    let app = app_with(vec![tiny_model()]).await;
    let client = app.as_user(Uuid::new_v4(), Uuid::new_v4());
    let t = triggered(&app, &client).await;
    let calls = provider_calls(&app);

    let outcome = app.services.summaries.run(t.payload.clone(), 1).await;
    assert_eq!(
        outcome,
        HandlerOutcome::Reject("model_unavailable".to_owned())
    );

    app.policy
        .set_catalog(vec![tiny_model(), disabled(summary_model())]);
    let outcome = app.services.summaries.run(t.payload.clone(), 1).await;
    assert_eq!(
        outcome,
        HandlerOutcome::Reject("model_unavailable".to_owned())
    );
    assert_eq!(provider_calls(&app), calls);
    assert!(summary_row(&app, t.chat).await.is_none());
}

#[tokio::test]
async fn existing_summary_without_truncation_does_not_trigger() {
    let app = app().await;
    let client = app.as_user(Uuid::new_v4(), Uuid::new_v4());
    let chat = create_chat(&client, TINY).await;
    let (first, _) = send(&app, &client, chat, "first question", "first answer").await;
    let upto = turn_message(&app, first, "assistant").await;
    insert_summary(&app, &client, chat, &upto, true).await;

    // Over the proactive threshold, nothing truncated, a summary exists.
    let (_, started) = send(
        &app,
        &client,
        chat,
        &big("second question"),
        "second answer",
    )
    .await;
    assert!(started["thread_summary_applied"].is_object(), "{started}");
    assert!(summary_payloads(&app).await.is_empty());
}

#[tokio::test]
async fn urgent_trigger_with_existing_summary_uses_base_frontier() {
    let app = app().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, TINY).await;
    let (first, _) = send(&app, &client, chat, "first question", "first answer").await;
    let upto = turn_message(&app, first, "assistant").await;
    insert_summary(&app, &client, chat, &upto, true).await;
    let (second, _) = send(
        &app,
        &client,
        chat,
        &big("second question"),
        "second answer",
    )
    .await;
    assert!(summary_payloads(&app).await.is_empty());

    // The second big turn no longer fits next to the third: truncation.
    send(&app, &client, chat, &big("third question"), "third answer").await;
    let payloads = summary_payloads(&app).await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let target = turn_message(&app, second, "assistant").await;
    assert_eq!(
        payloads[0].base_frontier(),
        Some((upto.created_at, upto.id))
    );
    assert_eq!(payloads[0].frozen_target(), (target.created_at, target.id));
}

#[tokio::test]
async fn disabled_worker_no_trigger() {
    let app = TestApp::builder()
        .catalog(vec![tiny_model(), summary_model()])
        .config(|c| c.thread_summary_worker.enabled = false)
        .build()
        .await;
    let client = app.as_user(Uuid::new_v4(), Uuid::new_v4());
    let chat = create_chat(&client, TINY).await;
    send(&app, &client, chat, "first question", "first answer").await;
    send(
        &app,
        &client,
        chat,
        &big("second question"),
        "second answer",
    )
    .await;
    assert!(summary_payloads(&app).await.is_empty());
}

#[tokio::test]
async fn startup_check_reports_missing_summary_model() {
    let app = app().await;
    assert!(app.services.summaries.check_summary_model().await);
    app.policy
        .set_catalog(vec![tiny_model(), disabled(summary_model())]);
    assert!(!app.services.summaries.check_summary_model().await);
    app.policy.set_catalog(vec![tiny_model()]);
    assert!(!app.services.summaries.check_summary_model().await);

    let off = TestApp::builder()
        .catalog(vec![tiny_model()])
        .config(|c| c.thread_summary_worker.enabled = false)
        .build()
        .await;
    assert!(
        off.services.summaries.check_summary_model().await,
        "not checked when disabled"
    );
}

#[tokio::test]
async fn outbox_pipeline_runs_the_summary_task() {
    let app = TestApp::builder()
        .catalog(vec![tiny_model(), summary_model()])
        .real_handlers()
        .build()
        .await;
    let client = app.as_user(Uuid::new_v4(), Uuid::new_v4());
    let chat = create_chat(&client, TINY).await;
    send(&app, &client, chat, "first question", "first answer").await;
    send(
        &app,
        &client,
        chat,
        &big("second question"),
        "second answer",
    )
    .await;
    // A delivery that ran before this reply was scripted is retried.
    app.oagw.push_json(
        PROVIDER_PATH,
        200,
        summary_response("<summary>From the pipeline</summary>", 7, 0),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let row = loop {
        if let Some(row) = summary_row(&app, chat).await {
            break row;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "summary not committed by the pipeline"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(row.summary_text, "From the pipeline");
    assert_eq!(row.token_estimate, 7);
}

#[tokio::test]
async fn second_summary_merges_and_advances_frontier() {
    let app = app().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let client = app.as_user(user, tenant);
    let t = summarized(&app, &client).await;
    let first = summary_row(&app, t.chat).await.unwrap();
    send(&app, &client, t.chat, "third question", "third answer").await;

    let target = turn_message(&app, t.second, "assistant").await;
    let second_user = turn_message(&app, t.second, "user").await;
    let payload = ThreadSummaryPayload::new(
        tenant,
        t.chat,
        Some((
            first.summarized_up_to_created_at,
            first.summarized_up_to_message_id,
        )),
        (target.created_at, target.id),
    );
    app.oagw.push_json(
        PROVIDER_PATH,
        200,
        summary_response("<summary>S2</summary>", 0, 0),
    );
    let outcome = app.services.summaries.run(payload.clone(), 1).await;
    assert_eq!(outcome, HandlerOutcome::Ok);

    let row = summary_row(&app, t.chat).await.unwrap();
    assert_eq!(row.id, first.id, "the row is updated in place");
    assert_eq!(row.summary_text, "S2");
    assert_eq!(
        (
            row.summarized_up_to_created_at,
            row.summarized_up_to_message_id
        ),
        (target.created_at, target.id)
    );
    // No usable output token count: ceil(2 bytes / 4).
    assert_eq!(row.token_estimate, 1);
    let compressed = compressed_ids(&app, t.chat).await;
    assert_eq!(compressed.len(), 4, "{compressed:?}");
    assert!(compressed.contains(&second_user.id) && compressed.contains(&target.id));
    let usage = system_usage_events(&app).await;
    assert_eq!(usage.len(), 2);
    assert!(
        usage
            .iter()
            .any(|e| e["request_id"] == payload.system_request_id.to_string())
    );
}
