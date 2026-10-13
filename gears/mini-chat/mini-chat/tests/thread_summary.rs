//! Thread summaries (spec §14; DESIGN §3.6 "Thread Summary Update", "Thread
//! Summary - Stable Range and Commit Invariant", §3.2 "System Task Attribution
//! Rules"): trigger at finalization, the outbox handler and the use of the
//! summary in the next turn's context.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Utc};
use mini_chat::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use mini_chat::domain::services::thread_summary::ThreadSummaryService;
use mini_chat::infra::db::entities::{message, thread_summary};
use mini_chat::infra::llm::LlmUsage;
use mini_chat::infra::outbox::payloads::ThreadSummaryPayload;
use mini_chat::infra::outbox::{QueueKind, ThreadSummaryHandler};
use mini_chat::testing::catalog::{self, standard_model};
use mini_chat::testing::{TestApp, TestUser};
use mini_chat_sdk::ModelCatalogEntry;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};
use serde_json::{Value, json};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{
    AccessScope, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
/// A user whose id differs from the platform default subject (the system identity).
const U: TestUser = TestUser::A2;
const SYSTEM_SUBJECT: &str = "11111111-6a88-4768-9dfc-6bcd5187d9ed";
const SUMMARY_MODEL: &str = "gpt-summary";
const SUMMARY_MAX_OUTPUT: u32 = 777;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

/// Default catalog with a small chat context window (effective budget 2000, so
/// two 2200-byte user messages reach the 80 % threshold) plus the summary model.
fn small_catalog() -> Vec<ModelCatalogEntry> {
    let mut models = catalog::default_catalog();
    for m in &mut models {
        m.context_window = 3000;
        m.max_output_tokens = 1000;
    }
    let mut summary = standard_model(SUMMARY_MODEL);
    summary.max_output_tokens = SUMMARY_MAX_OUTPUT;
    models.push(summary);
    models
}

async fn app_with(
    catalog: Vec<ModelCatalogEntry>,
    enabled: bool,
    summary_model: &'static str,
) -> TestApp {
    TestApp::builder()
        .catalog(catalog)
        .config(move |c| {
            c.thread_summary_worker.enabled = enabled;
            c.thread_summary_worker.max_attempts = 2;
            summary_model.clone_into(&mut c.thread_summary_worker.summary_model_id);
        })
        .build()
        .await
}

/// Trigger enabled: summaries run through the outbox pipeline.
async fn auto_app() -> TestApp {
    app_with(small_catalog(), true, SUMMARY_MODEL).await
}

/// Trigger disabled: the tests drive the handler themselves.
async fn manual_app() -> TestApp {
    app_with(small_catalog(), false, SUMMARY_MODEL).await
}

fn long_text() -> String {
    "x".repeat(2200)
}

async fn create_chat(app: &TestApp) -> Uuid {
    let r = app.call(U, Method::POST, CHATS, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

fn stream_path(chat: Uuid) -> String {
    format!("{CHATS}/{chat}/messages:stream")
}

/// Send `content`, wait for `done`, return the request id and the events.
async fn send(app: &TestApp, chat: Uuid, content: &str) -> (Uuid, Vec<(String, Value)>) {
    let request_id = Uuid::new_v4();
    let c = app
        .stream(
            U,
            &stream_path(chat),
            json!({"content": content, "request_id": request_id}),
        )
        .await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    assert_eq!(c.last().unwrap().0, "done", "{c:?}");
    (request_id, c.events)
}

async fn messages(app: &TestApp, chat: Uuid) -> Vec<message::Model> {
    let conn = app.db.conn().unwrap();
    message::Entity::find()
        .filter(message::Column::ChatId.eq(chat))
        .order_by_asc(message::Column::CreatedAt)
        .order_by_asc(message::Column::Id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

fn message_of(msgs: &[message::Model], request_id: Uuid, role: &str) -> message::Model {
    msgs.iter()
        .find(|m| m.request_id == Some(request_id) && m.role == role)
        .cloned()
        .expect("message of request")
}

async fn summary_row(app: &TestApp, chat: Uuid) -> Option<thread_summary::Model> {
    let conn = app.db.conn().unwrap();
    thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
}

/// Seed a summary whose frontier is `frontier` (messages up to it compressed).
async fn seed_summary(app: &TestApp, chat: Uuid, frontier: &message::Model, text: &str) {
    let now = Utc::now();
    let conn = app.db.conn().unwrap();
    secure_insert::<thread_summary::Entity>(
        thread_summary::Model {
            id: Uuid::new_v4(),
            tenant_id: U.tenant_id,
            chat_id: chat,
            summary_text: Some(text.to_owned()),
            summarized_up_to_created_at: frontier.created_at,
            summarized_up_to_message_id: frontier.id,
            token_estimate: Some(9),
            created_at: now,
            updated_at: now,
        }
        .into_active_model(),
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(message::Column::ChatId.eq(chat))
        .filter(message::Column::CreatedAt.lte(frontier.created_at))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

async fn soft_delete_message(app: &TestApp, id: Uuid) {
    let conn = app.db.conn().unwrap();
    message::Entity::update_many()
        .col_expr(message::Column::DeletedAt, Expr::value(Some(Utc::now())))
        .filter(message::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

/// Usage events delivered so far with `billing_outcome = system_task`, after
/// waiting (up to 5 s) for at least `n` of them.
async fn system_usage(app: &TestApp, n: usize) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let found: Vec<Value> = app
            .outbox_messages_n(QueueKind::Usage, 0)
            .await
            .into_iter()
            .filter(|e| e["billing_outcome"] == "system_task")
            .collect();
        if found.len() >= n || tokio::time::Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Non-streaming (summary) provider requests.
fn summary_requests(app: &TestApp) -> Vec<Value> {
    app.provider
        .chat_requests()
        .into_iter()
        .filter(|r| r["stream"] == false)
        .collect()
}

/// Text of an input item (plain string or content-array form).
fn input_text(item: &Value) -> String {
    match &item["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        other => panic!("unexpected content {other}"),
    }
}

fn handler(app: &TestApp) -> ThreadSummaryHandler {
    ThreadSummaryHandler::new(Arc::clone(&app.services.thread_summary))
}

fn outbox_message(payload: &ThreadSummaryPayload, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload: serde_json::to_vec(payload).unwrap(),
        payload_type: "mini_chat.thread_summary.v1".to_owned(),
        created_at: Utc::now(),
        attempts,
    }
}

/// Two turns, then the payload the second (causing) turn would enqueue.
async fn two_turns_and_payload(app: &TestApp) -> (Uuid, Uuid, Uuid, ThreadSummaryPayload) {
    let chat = create_chat(app).await;
    let (first, _) = send(app, chat, "first question").await;
    let (second, _) = send(app, chat, "second question").await;
    let conn = app.db.conn().unwrap();
    let payload = ThreadSummaryService::build_payload(&conn, U.tenant_id, chat, second)
        .await
        .unwrap()
        .expect("a summary payload");
    (chat, first, second, payload)
}

fn is_reject(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Reject(_))
}

fn is_retry(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Retry)
}

fn is_ok(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Ok)
}

/// Wait until the fake provider received `n` summary requests.
async fn wait_summary_requests(app: &TestApp, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while summary_requests(app).len() < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "summary request not received"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// trigger
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn completed_turn_enqueues_summary_with_frozen_target_excluding_causing_turn() {
    let app = auto_app().await;
    let chat = create_chat(&app).await;
    let (first, _) = send(&app, chat, &long_text()).await;
    // The first turn stays below the threshold: nothing is scheduled.
    app.assert_no_outbox(QueueKind::ThreadSummary, Duration::from_millis(300))
        .await;

    let (second, _) = send(&app, chat, &long_text()).await;
    let payloads = app.outbox_messages(QueueKind::ThreadSummary).await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let p = &payloads[0];

    let msgs = messages(&app, chat).await;
    let target = message_of(&msgs, first, "assistant");
    assert_eq!(p["tenant_id"], json!(U.tenant_id));
    assert_eq!(p["chat_id"], json!(chat));
    assert_eq!(p["system_task_type"], "thread_summary_update");
    assert!(Uuid::parse_str(p["system_request_id"].as_str().unwrap()).is_ok());
    assert_eq!(p["base_frontier_created_at"], Value::Null);
    assert_eq!(p["base_frontier_message_id"], Value::Null);
    assert_eq!(p["frozen_target_message_id"], json!(target.id));
    let target_at: DateTime<Utc> = p["frozen_target_created_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(target_at, target.created_at);
    // The causing turn's messages are never the target.
    assert!(
        msgs.iter()
            .filter(|m| m.request_id == Some(second))
            .all(|m| json!(m.id) != p["frozen_target_message_id"])
    );
}

#[tokio::test]
async fn disabled_worker_enqueues_nothing() {
    let app = manual_app().await;
    let chat = create_chat(&app).await;
    send(&app, chat, &long_text()).await;
    send(&app, chat, &long_text()).await;
    app.assert_no_outbox(QueueKind::ThreadSummary, Duration::from_millis(300))
        .await;
}

// ---------------------------------------------------------------------------------------------
// handler
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn handler_commits_summary_marks_compressed_and_emits_system_usage_event() {
    let app = auto_app().await;
    app.provider.push_completion(
        "<analysis>reasoning</analysis>\n<summary>\nThe summary.\n\n\n\nSecond part.\n</summary>",
        Some(LlmUsage {
            input_tokens: 50,
            output_tokens: 30,
            cache_read_input_tokens: 4,
            cache_write_input_tokens: 0,
            reasoning_tokens: 10,
        }),
    );
    let chat = create_chat(&app).await;
    let (first, _) = send(&app, chat, &long_text()).await;
    let (second, _) = send(&app, chat, &long_text()).await;

    let events = system_usage(&app, 1).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let payload = &app.outbox_messages(QueueKind::ThreadSummary).await[0];
    let sys_id = Uuid::parse_str(payload["system_request_id"].as_str().unwrap()).unwrap();
    let ev = &events[0];
    let obj = ev.as_object().unwrap();
    assert!(!obj.contains_key("user_id"), "{ev}");
    assert!(!obj.contains_key("turn_id"), "{ev}");
    assert_eq!(ev["tenant_id"], json!(U.tenant_id));
    assert_eq!(ev["chat_id"], json!(chat));
    assert_eq!(ev["request_id"], json!(sys_id));
    assert_eq!(ev["effective_model"], SUMMARY_MODEL);
    assert_eq!(ev["selected_model"], SUMMARY_MODEL);
    assert_eq!(ev["terminal_state"], "completed");
    assert_eq!(ev["billing_outcome"], "system_task");
    assert_eq!(ev["settlement_method"], "none");
    assert_eq!(ev["actual_credits_micro"], 0);
    assert_eq!(ev["policy_version_applied"], 0);
    assert_eq!(ev["requester_type"], "system");
    assert_eq!(ev["system_task_type"], "thread_summary_update");
    assert_eq!(ev["web_search_calls"], 0);
    assert_eq!(ev["code_interpreter_calls"], 0);
    assert_eq!(ev["file_search_calls"], 0);
    assert_eq!(
        ev["usage"],
        json!({"input_tokens": 50, "output_tokens": 30, "cache_read_input_tokens": 4,
               "cache_write_input_tokens": 0, "reasoning_tokens": 10})
    );
    assert_eq!(
        ev["dedupe_key"],
        format!(
            "{}/thread_summary_update/{}",
            U.tenant_id.simple(),
            sys_id.simple()
        )
    );
    assert!(ev["timestamp"].as_str().unwrap().ends_with('Z'), "{ev}");

    // Committed state.
    let msgs = messages(&app, chat).await;
    let target = message_of(&msgs, first, "assistant");
    let row = summary_row(&app, chat).await.expect("summary committed");
    assert_eq!(
        row.summary_text.as_deref(),
        Some("The summary.\n\nSecond part.")
    );
    assert_eq!(row.summarized_up_to_message_id, target.id);
    assert_eq!(row.summarized_up_to_created_at, target.created_at);
    assert_eq!(row.token_estimate, Some(20));
    assert_eq!(row.tenant_id, U.tenant_id);
    for m in &msgs {
        assert_eq!(
            m.is_compressed,
            m.request_id == Some(first),
            "message {} ({:?})",
            m.role,
            m.request_id
        );
    }
    assert!(msgs.iter().any(|m| m.request_id == Some(second)));

    // The summary request.
    let reqs = summary_requests(&app);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    let r = &reqs[0];
    assert_eq!(r["stream"], false);
    assert_eq!(r["model"], SUMMARY_MODEL);
    assert_eq!(r["max_output_tokens"], SUMMARY_MAX_OUTPUT);
    assert_eq!(r["instructions"], DEFAULT_SUMMARY_SYSTEM_PROMPT);
    assert_eq!(r["metadata"]["request_type"], "summary");
    assert_eq!(r["metadata"]["feature"], "none");
    assert_eq!(r["metadata"]["tenant_id"], U.tenant_id.to_string());
    assert_eq!(r["metadata"]["user_id"], SYSTEM_SUBJECT);
    assert_eq!(r["metadata"]["chat_id"], chat.to_string());
    assert_eq!(
        r["user"],
        format!(
            "{}{}",
            U.tenant_id.simple(),
            Uuid::parse_str(SYSTEM_SUBJECT).unwrap().simple()
        )
    );
    assert!(r.get("tools").is_none(), "{r}");
    let input = r["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "{r}");
    assert_eq!(input[0]["role"], "user");
    let prompt = input_text(&input[0]);
    assert!(
        prompt.starts_with(&format!(
            "Summarize the following conversation:\n\nUser: {}\n\nAssistant: Hello\n\n",
            long_text()
        )),
        "{prompt}"
    );
    assert!(
        prompt.ends_with("followed by a <summary> block."),
        "{prompt}"
    );
    // The gear's S2S context carries the call.
    let recorded = app
        .provider
        .requests()
        .into_iter()
        .rfind(|r| r.json.as_ref().is_some_and(|j| j["stream"] == false))
        .unwrap();
    assert_eq!(recorded.subject_id, TestUser::S2S.user_id);
}

#[tokio::test]
async fn next_turn_uses_summary_and_stream_started_reports_thread_summary_applied() {
    let app = auto_app().await;
    app.provider.push_completion(
        "<summary>Earlier: two long questions.</summary>",
        Some(LlmUsage {
            input_tokens: 40,
            output_tokens: 12,
            ..LlmUsage::default()
        }),
    );
    let chat = create_chat(&app).await;
    send(&app, chat, &long_text()).await;
    let (second, _) = send(&app, chat, &long_text()).await;
    assert_eq!(system_usage(&app, 1).await.len(), 1);

    let (_, events) = send(&app, chat, "next").await;
    let started = &events[0];
    assert_eq!(started.0, "stream_started");
    assert_eq!(started.1["thread_summary_applied"]["token_estimate"], 12);

    let req = app
        .provider
        .chat_requests()
        .into_iter()
        .rfind(|r| r["stream"] == true)
        .unwrap();
    let input = req["input"].as_array().unwrap();
    let texts: Vec<String> = input.iter().map(input_text).collect();
    assert_eq!(
        texts[0],
        "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.\n\nEarlier: two long questions."
    );
    assert_eq!(input[0]["role"], "user");
    // Recent messages start after the frontier: the second turn, then the new message.
    let msgs = messages(&app, chat).await;
    assert_eq!(
        texts[1..],
        [
            message_of(&msgs, second, "user").content,
            "Hello".to_owned(),
            "next".to_owned()
        ]
    );
    // A kept summary without truncation does not schedule another run.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(app.outbox_messages(QueueKind::ThreadSummary).await.len(), 1);
}

#[tokio::test]
async fn provider_failure_retries_then_rejects_at_max_attempts_keeping_old_summary() {
    let app = manual_app().await;
    let chat = create_chat(&app).await;
    let (first, _) = send(&app, chat, "first question").await;
    let msgs = messages(&app, chat).await;
    let first_user = message_of(&msgs, first, "user");
    seed_summary(&app, chat, &first_user, "old summary").await;
    let (second, _) = send(&app, chat, "second question").await;
    let conn = app.db.conn().unwrap();
    let payload = ThreadSummaryService::build_payload(&conn, U.tenant_id, chat, second)
        .await
        .unwrap()
        .expect("payload");
    assert_eq!(payload.base_frontier_message_id, Some(first_user.id));
    let before = summary_row(&app, chat).await.unwrap();

    let h = handler(&app);
    app.provider.fail_next("/v1/responses", 500);
    assert!(is_retry(&h.handle(&outbox_message(&payload, 0)).await));
    app.provider.fail_next("/v1/responses", 500);
    // attempts are 0-based: the 2nd delivery is the last one (max_attempts = 2).
    assert!(is_reject(&h.handle(&outbox_message(&payload, 1)).await));

    assert_eq!(summary_requests(&app).len(), 2);
    assert_eq!(summary_row(&app, chat).await.unwrap(), before);
    let target = message_of(&messages(&app, chat).await, first, "assistant");
    assert!(!target.is_compressed);
    assert!(system_usage(&app, 0).await.is_empty());

    // The existing summary is merged into the prompt.
    let prompt = input_text(&summary_requests(&app)[0]["input"][0]);
    assert!(
        prompt.contains("<existing_summary>\nold summary\n</existing_summary>"),
        "{prompt}"
    );
    assert!(prompt.contains("Assistant: Hello"), "{prompt}");
    assert!(!prompt.contains("first question"), "{prompt}");
}

#[tokio::test]
async fn empty_summary_retries() {
    let app = manual_app().await;
    let (chat, _, _, payload) = two_turns_and_payload(&app).await;
    app.provider
        .push_completion("<analysis>only reasoning</analysis>", None);
    let r = handler(&app).handle(&outbox_message(&payload, 0)).await;
    assert!(is_retry(&r), "{r:?}");
    assert!(summary_row(&app, chat).await.is_none());
    assert!(messages(&app, chat).await.iter().all(|m| !m.is_compressed));
    assert!(system_usage(&app, 0).await.is_empty());
}

#[tokio::test]
async fn missing_summary_model_rejects() {
    // Missing from the catalog.
    let app = app_with(small_catalog(), false, "no-such-model").await;
    let (chat, _, _, payload) = two_turns_and_payload(&app).await;
    let r = handler(&app).handle(&outbox_message(&payload, 0)).await;
    assert!(is_reject(&r), "{r:?}");
    assert!(summary_requests(&app).is_empty());
    assert!(summary_row(&app, chat).await.is_none());

    // Disabled in the catalog.
    let mut models = small_catalog();
    models
        .iter_mut()
        .find(|m| m.id == SUMMARY_MODEL)
        .unwrap()
        .enabled = false;
    let app = app_with(models, false, SUMMARY_MODEL).await;
    let (chat, _, _, payload) = two_turns_and_payload(&app).await;
    let r = handler(&app).handle(&outbox_message(&payload, 0)).await;
    assert!(is_reject(&r), "{r:?}");
    assert!(summary_requests(&app).is_empty());
    assert!(summary_row(&app, chat).await.is_none());
}

#[tokio::test]
async fn malformed_payload_rejected() {
    let app = manual_app().await;
    let msg = OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload: b"{not json".to_vec(),
        payload_type: "mini_chat.thread_summary.v1".to_owned(),
        created_at: Utc::now(),
        attempts: 0,
    };
    assert!(is_reject(&handler(&app).handle(&msg).await));
}

#[tokio::test]
async fn cas_conflict_does_not_double_commit() {
    let app = manual_app().await;
    let (chat, first, _, payload) = two_turns_and_payload(&app).await;
    app.provider
        .push_completion("<summary>first commit</summary>", None);
    app.provider
        .push_completion("<summary>late commit</summary>", None);

    // A: its provider call is held until B has committed.
    app.provider.hold_next("/v1/responses");
    let h = Arc::new(handler(&app));
    let a = {
        let h = Arc::clone(&h);
        let msg = outbox_message(&payload, 0);
        tokio::spawn(async move { h.handle(&msg).await })
    };
    wait_summary_requests(&app, 1).await;
    // B: same frozen range, commits first.
    let b = h.handle(&outbox_message(&payload, 0)).await;
    assert!(is_ok(&b), "{b:?}");
    app.provider.release_held();
    let a = a.await.unwrap();
    assert!(is_ok(&a), "{a:?}");

    let row = summary_row(&app, chat).await.unwrap();
    assert_eq!(row.summary_text.as_deref(), Some("first commit"));
    assert_eq!(system_usage(&app, 1).await.len(), 1);

    // A replay after the commit stops at the pre-check (no provider call).
    let c = h.handle(&outbox_message(&payload, 3)).await;
    assert!(is_ok(&c), "{c:?}");
    assert_eq!(summary_requests(&app).len(), 2);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(system_usage(&app, 1).await.len(), 1);
    assert_eq!(
        summary_row(&app, chat)
            .await
            .unwrap()
            .summary_text
            .as_deref(),
        Some("first commit")
    );
    let target = message_of(&messages(&app, chat).await, first, "assistant");
    assert!(target.is_compressed);
}

#[tokio::test]
async fn frontier_deleted_skips_commit() {
    let app = manual_app().await;
    let (chat, first, _, payload) = two_turns_and_payload(&app).await;
    let target = message_of(&messages(&app, chat).await, first, "assistant");
    assert_eq!(payload.frozen_target_message_id, target.id);

    app.provider.hold_next("/v1/responses");
    let h = Arc::new(handler(&app));
    let run = {
        let h = Arc::clone(&h);
        let msg = outbox_message(&payload, 0);
        tokio::spawn(async move { h.handle(&msg).await })
    };
    wait_summary_requests(&app, 1).await;
    // The target is deleted while the summary is generated.
    soft_delete_message(&app, target.id).await;
    app.provider.release_held();
    let r = run.await.unwrap();
    assert!(is_ok(&r), "{r:?}");

    assert!(summary_row(&app, chat).await.is_none());
    assert!(messages(&app, chat).await.iter().all(|m| !m.is_compressed));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(system_usage(&app, 0).await.is_empty());
}

#[tokio::test]
async fn base_missing_skips_without_provider_call() {
    let app = manual_app().await;
    let chat = create_chat(&app).await;
    let (first, _) = send(&app, chat, "first question").await;
    let first_user = message_of(&messages(&app, chat).await, first, "user");
    seed_summary(&app, chat, &first_user, "old summary").await;
    let (second, _) = send(&app, chat, "second question").await;
    let conn = app.db.conn().unwrap();
    let payload = ThreadSummaryService::build_payload(&conn, U.tenant_id, chat, second)
        .await
        .unwrap()
        .expect("payload");
    // A mutation dropped the summary the task was based on.
    thread_summary::Entity::delete_many()
        .filter(thread_summary::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    let r = handler(&app).handle(&outbox_message(&payload, 0)).await;
    assert!(is_ok(&r), "{r:?}");
    assert!(summary_requests(&app).is_empty());
    assert!(summary_row(&app, chat).await.is_none());
}

#[tokio::test]
async fn build_payload_skips_when_nothing_precedes_the_causing_turn() {
    let app = manual_app().await;
    let chat = create_chat(&app).await;
    let (first, _) = send(&app, chat, "only question").await;
    let conn = app.db.conn().unwrap();
    assert!(
        ThreadSummaryService::build_payload(&conn, U.tenant_id, chat, first)
            .await
            .unwrap()
            .is_none()
    );

    // Frontier already at the target.
    let (second, _) = send(&app, chat, "second").await;
    let target = message_of(&messages(&app, chat).await, first, "assistant");
    seed_summary(&app, chat, &target, "covers the first turn").await;
    assert!(
        ThreadSummaryService::build_payload(&conn, U.tenant_id, chat, second)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn startup_check_reports_summary_model_availability() {
    let app = manual_app().await;
    assert!(app.services.thread_summary.check_summary_model().await);
    let app = app_with(small_catalog(), false, "no-such-model").await;
    assert!(!app.services.thread_summary.check_summary_model().await);
}

// ---------------------------------------------------------------------------------------------
// prompt-too-long retry
// ---------------------------------------------------------------------------------------------

fn context_length_error() -> Value {
    json!({"error": {
        "message": "This model's maximum context length is 1000 tokens. However, your messages resulted in 5000 tokens.",
        "type": "invalid_request_error",
        "code": "context_length_exceeded"
    }})
}

/// Message entries (`User: ` / `Assistant: `) of a summary request's prompt.
fn prompt_entries(req: &Value) -> Vec<String> {
    input_text(&req["input"][0])
        .split("\n\n")
        .filter(|part| part.starts_with("User: ") || part.starts_with("Assistant: "))
        .map(str::to_owned)
        .collect()
}

/// Five turns (`q1`..`q5`); the payload of the fifth covers the first four (8 messages).
async fn five_turns_and_payload(app: &TestApp) -> (Uuid, ThreadSummaryPayload) {
    let chat = create_chat(app).await;
    let mut last = Uuid::nil();
    for i in 1..=5 {
        last = send(app, chat, &format!("q{i}")).await.0;
    }
    let conn = app.db.conn().unwrap();
    let payload = ThreadSummaryService::build_payload(&conn, U.tenant_id, chat, last)
        .await
        .unwrap()
        .expect("payload");
    (chat, payload)
}

#[tokio::test]
async fn prompt_too_long_retry_drops_oldest_fifth_and_commits() {
    let app = manual_app().await;
    let (chat, payload) = five_turns_and_payload(&app).await;
    app.provider
        .push_completion_error(400, context_length_error());
    app.provider
        .push_completion("<summary>fits now</summary>", None);

    let r = handler(&app).handle(&outbox_message(&payload, 0)).await;
    assert!(is_ok(&r), "{r:?}");

    let reqs = summary_requests(&app);
    assert_eq!(reqs.len(), 2);
    let first = prompt_entries(&reqs[0]);
    let second = prompt_entries(&reqs[1]);
    assert_eq!(first.len(), 8, "{first:?}");
    assert_eq!(first[0], "User: q1");
    // ceil(8 / 5) = 2 oldest messages dropped.
    assert_eq!(second, first[2..].to_vec());

    let row = summary_row(&app, chat).await.expect("committed");
    assert_eq!(row.summary_text.as_deref(), Some("fits now"));
    assert_eq!(
        row.summarized_up_to_message_id,
        payload.frozen_target_message_id
    );
    // The whole frozen range is compressed, dropped messages included.
    let compressed = messages(&app, chat)
        .await
        .iter()
        .filter(|m| m.is_compressed)
        .count();
    assert_eq!(compressed, 8);
    assert_eq!(system_usage(&app, 1).await.len(), 1);
}

#[tokio::test]
async fn prompt_too_long_gives_up_after_two_retries() {
    let app = manual_app().await;
    let (chat, payload) = five_turns_and_payload(&app).await;
    for _ in 0..3 {
        app.provider
            .push_completion_error(400, context_length_error());
    }
    app.provider
        .push_completion("<summary>never used</summary>", None);

    let r = handler(&app).handle(&outbox_message(&payload, 0)).await;
    assert!(is_retry(&r), "{r:?}");
    let counts: Vec<usize> = summary_requests(&app)
        .iter()
        .map(|r| prompt_entries(r).len())
        .collect();
    assert_eq!(counts, [8, 6, 4]);
    assert!(summary_row(&app, chat).await.is_none());
    assert!(messages(&app, chat).await.iter().all(|m| !m.is_compressed));
    assert!(system_usage(&app, 0).await.is_empty());
}

#[tokio::test]
async fn prompt_too_long_never_drops_below_two_messages() {
    let app = manual_app().await;
    // The range is the first turn: two messages, nothing left to drop.
    let (chat, _, _, payload) = two_turns_and_payload(&app).await;
    app.provider
        .push_completion_error(400, context_length_error());

    let r = handler(&app).handle(&outbox_message(&payload, 0)).await;
    assert!(is_retry(&r), "{r:?}");
    let reqs = summary_requests(&app);
    assert_eq!(reqs.len(), 1);
    assert_eq!(prompt_entries(&reqs[0]).len(), 2);
    assert!(summary_row(&app, chat).await.is_none());
}
