//! `POST /v1/chats/{id}/messages:stream` and `GET /v1/chats/{id}/turns/{request_id}`
//! (DESIGN §3.3 "Streaming Contract", "Turn Status API", §3.6 "Send Message with
//! Streaming Response", §4 "Turn Lifecycle", §5.7).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::http::{Method, StatusCode};
use chrono::Utc;
use mini_chat::domain::clock::now_utc;
use mini_chat::domain::estimation::period_starts;
use mini_chat::domain::model::{PeriodType, TurnState};
use mini_chat::infra::db::entities::{chat, chat_turn, chat_vector_store, message, quota_usage};
use mini_chat::infra::db::repos::TurnRepo;
use mini_chat::infra::db::repos::turn::{TerminalUpdate, TurnCounters};
use mini_chat::infra::outbox::QueueKind;
use mini_chat::testing::catalog::{self, VISION_INPUT};
use mini_chat::testing::fake_provider::{FAKE_ITEM_ID, completed_event, delta_event};
use mini_chat::testing::seed::{self, NewAttachment};
use mini_chat::testing::{ScriptedStream, SseCapture, TestApp, TestUser, midnight_safe};
use mini_chat_sdk::{KillSwitches, TierLimits};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};
use serde_json::{Value, json};
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
const U: TestUser = TestUser::A1;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

async fn app() -> TestApp {
    TestApp::builder().build().await
}

async fn create_chat(app: &TestApp, user: TestUser) -> Uuid {
    let r = app.call(user, Method::POST, CHATS, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

fn stream_path(chat: Uuid) -> String {
    format!("{CHATS}/{chat}/messages:stream")
}

fn turn_path(chat: Uuid, request_id: Uuid) -> String {
    format!("{CHATS}/{chat}/turns/{request_id}")
}

async fn send(app: &TestApp, chat: Uuid, body: Value) -> SseCapture {
    app.stream(U, &stream_path(chat), body).await
}

fn uuid_of(v: &Value) -> Uuid {
    Uuid::parse_str(v.as_str().expect("uuid string")).expect("uuid")
}

/// `(name, data)` with `data.type = name` (Responses API event).
fn ev(name: &str, mut data: Value) -> (String, Value) {
    data["type"] = json!(name);
    (name.to_owned(), data)
}

fn created() -> (String, Value) {
    ev(
        "response.created",
        json!({"response": {"id": "resp_fake0001", "status": "in_progress", "output": []}}),
    )
}

fn web_search_start(n: u32) -> (String, Value) {
    ev(
        "response.web_search_call.searching",
        json!({"item_id": format!("ws_{n}"), "output_index": 0}),
    )
}

fn web_search_done(n: u32) -> (String, Value) {
    ev(
        "response.web_search_call.completed",
        json!({"item_id": format!("ws_{n}"), "output_index": 0}),
    )
}

async fn turns(app: &TestApp, chat: Uuid) -> Vec<chat_turn::Model> {
    let conn = app.db.conn().unwrap();
    chat_turn::Entity::find()
        .filter(chat_turn::Column::ChatId.eq(chat))
        .order_by_asc(chat_turn::Column::StartedAt)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

async fn turn_of(app: &TestApp, chat: Uuid, request_id: Uuid) -> chat_turn::Model {
    turns(app, chat)
        .await
        .into_iter()
        .find(|t| t.request_id == request_id)
        .expect("turn of request")
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

async fn quota_rows(app: &TestApp, user: TestUser) -> Vec<quota_usage::Model> {
    let conn = app.db.conn().unwrap();
    let mut rows = quota_usage::Entity::find()
        .filter(quota_usage::Column::UserId.eq(user.user_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap();
    rows.sort_by(|a, b| (&a.bucket, &a.period_type).cmp(&(&b.bucket, &b.period_type)));
    rows
}

async fn chat_row(app: &TestApp, chat: Uuid) -> chat::Model {
    let conn = app.db.conn().unwrap();
    chat::Entity::find_by_id(chat)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .unwrap()
}

/// Polls `f` every 20 ms for up to 10 s.
async fn eventually<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !f().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_terminal(app: &TestApp, chat: Uuid, request_id: Uuid) -> chat_turn::Model {
    eventually("turn leaves running", || async {
        turns(app, chat)
            .await
            .iter()
            .any(|t| t.request_id == request_id && t.state != "running")
    })
    .await;
    turn_of(app, chat, request_id).await
}

fn assert_problem(c: &SseCapture, status: StatusCode) -> &Value {
    assert_eq!(c.status, status, "{c:?}");
    c.problem.as_ref().expect("problem body")
}

fn field_reason(p: &Value) -> (&str, &str) {
    let v = &p["context"]["field_violations"][0];
    (
        v["field"].as_str().unwrap_or_default(),
        v["reason"].as_str().unwrap_or_default(),
    )
}

async fn assert_nothing_written(app: &TestApp, chat: Uuid) {
    assert!(app.provider.chat_requests().is_empty(), "provider called");
    assert!(turns(app, chat).await.is_empty(), "turn row written");
    assert!(messages(app, chat).await.is_empty(), "message written");
    assert!(quota_rows(app, U).await.is_empty(), "reserve written");
}

fn tiny() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 1_000,
        limit_monthly_credits_micro: 1_000,
    }
}

fn large() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 100_000_000,
        limit_monthly_credits_micro: 1_000_000_000,
    }
}

/// Premium exhausted (its reserve is ~12 400 µcredits), standard plenty.
async fn premium_exhausted_app() -> TestApp {
    TestApp::builder().limits(large(), tiny()).build().await
}

fn no_kill_switches() -> KillSwitches {
    KillSwitches {
        disable_premium_tier: false,
        force_standard_tier: false,
        disable_web_search: false,
        disable_file_search: false,
        disable_images: false,
        disable_code_interpreter: false,
    }
}

// ---------------------------------------------------------------------------------------------
// happy path
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stream_happy_path_event_order_and_persistence() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let before = chat_row(&app, chat).await.updated_at;
    app.provider
        .push_stream(ScriptedStream::text(&["Hel", "lo"], 10, 5));
    let request_id = Uuid::new_v4();

    let c = send(
        &app,
        chat,
        json!({"content": "hi", "request_id": request_id}),
    )
    .await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    let ct = c.headers["content-type"].to_str().unwrap();
    assert!(ct.starts_with("text/event-stream"), "{ct}");
    assert_eq!(c.headers["cache-control"], "no-cache");
    assert_eq!(c.names(), ["stream_started", "delta", "delta", "done"]);

    let started = c.first("stream_started").unwrap();
    assert_eq!(started["is_new_turn"], true);
    assert_eq!(uuid_of(&started["request_id"]), request_id);
    assert!(started.get("thread_summary_applied").is_none(), "{started}");
    let message_id = uuid_of(&started["message_id"]);
    assert_eq!(c.events[1].1, json!({"type": "text", "content": "Hel"}));
    assert_eq!(c.events[2].1, json!({"type": "text", "content": "lo"}));

    let done = &c.events[3].1;
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 10, "output_tokens": 5})
    );
    assert_eq!(done["effective_model"], "gpt-premium");
    assert_eq!(done["selected_model"], "gpt-premium");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_from").is_none(), "{done}");
    assert!(done.get("downgrade_reason").is_none(), "{done}");
    let warnings = done["quota_warnings"].as_array().expect("quota_warnings");
    assert_eq!(warnings.len(), 4, "{done}");
    for w in warnings {
        assert!(w["tier"] == "premium" || w["tier"] == "total", "{w}");
        assert!(w["period"] == "daily" || w["period"] == "monthly", "{w}");
        assert_eq!(w["warning"], false, "{w}");
        assert_eq!(w["exhausted"], false, "{w}");
        assert!(w.get("next_reset").is_none(), "{w}");
        assert!(w["remaining_percentage"].as_u64().unwrap() <= 100);
    }

    let msgs = messages(&app, chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[0].content, "hi");
    assert_eq!(msgs[0].request_id, Some(request_id));
    assert_eq!(msgs[1].role, "assistant");
    assert_eq!(msgs[1].id, message_id);
    assert_eq!(msgs[1].content, "Hello");
    assert_eq!(msgs[1].request_id, Some(request_id));
    assert_eq!(msgs[1].model.as_deref(), Some("gpt-premium"));
    assert_eq!((msgs[1].input_tokens, msgs[1].output_tokens), (10, 5));

    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.assistant_message_id, Some(message_id));
    assert_eq!(turn.requester_type, "user");
    assert_eq!(turn.requester_user_id, Some(U.user_id));
    assert_eq!(turn.effective_model.as_deref(), Some("gpt-premium"));
    assert!(turn.reserve_tokens.is_some() && turn.completed_at.is_some());

    assert!(chat_row(&app, chat).await.updated_at > before);
    let r = app
        .call(U, Method::GET, &format!("{CHATS}/{chat}"), None)
        .await;
    assert_eq!(r.json["message_count"], 2, "{}", r.json);

    // Settled: nothing stays reserved.
    for row in quota_rows(&app, U).await {
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
    }
    let usage = app.outbox_messages_n(QueueKind::Usage, 1).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["billing_outcome"], "completed");
    assert_eq!(usage[0]["settlement_method"], "actual");
}

#[tokio::test]
async fn server_generates_request_id_when_omitted() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let c = send(&app, chat, json!({"content": "hi"})).await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    let request_id = uuid_of(&c.first("stream_started").unwrap()["request_id"]);
    assert_eq!(request_id.get_version_num(), 4);
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "completed");
    assert!(
        messages(&app, chat)
            .await
            .iter()
            .all(|m| m.request_id == Some(request_id))
    );
}

#[tokio::test]
async fn provider_request_contains_expected_fields() {
    let mut models = catalog::default_catalog();
    models[0].system_prompt = "You are a helpful assistant.".to_owned();
    models[0].provider_model_id = "gpt-premium-2026-01".to_owned();
    let app = TestApp::builder().catalog(models).build().await;
    let chat = create_chat(&app, U).await;
    let c = send(&app, chat, json!({"content": "hi there"})).await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");

    let reqs = app.provider.chat_requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r["model"], "gpt-premium-2026-01");
    assert!(
        r["instructions"]
            .as_str()
            .unwrap()
            .starts_with("You are a helpful assistant."),
        "{r}"
    );
    let input = r["input"].as_array().unwrap();
    assert_eq!(
        input.last().unwrap(),
        &json!({"role": "user", "content": [{"type": "input_text", "text": "hi there"}]})
    );
    assert_eq!(r["user"].as_str().unwrap().len(), 64);
    assert_eq!(
        r["user"],
        format!("{}{}", U.tenant_id.simple(), U.user_id.simple())
    );
    assert_eq!(r["metadata"]["request_type"], "chat");
    assert_eq!(r["metadata"]["chat_id"], chat.to_string());
    assert_eq!(r["metadata"]["feature"], "none");
    assert_eq!(r["max_output_tokens"], 4096);
    assert_eq!(r["stream"], true);
    assert_eq!(r["store"], false);
    assert!(r.get("tools").is_none(), "{r}");
    // Provider calls use the gear's S2S identity.
    let recorded = app.provider.requests();
    assert_eq!(recorded[0].path, "/api.openai.com/v1/responses");
}

#[tokio::test]
async fn history_carried_into_second_turn_and_message_order() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider
        .push_stream(ScriptedStream::text(&["A1"], 10, 5));
    app.provider
        .push_stream(ScriptedStream::text(&["A2"], 10, 5));
    assert_eq!(send(&app, chat, json!({"content": "q1"})).await.status, 200);
    assert_eq!(send(&app, chat, json!({"content": "q2"})).await.status, 200);

    let reqs = app.provider.chat_requests();
    let input = reqs[1]["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "{input:?}");
    assert_eq!(input[0], json!({"role": "user", "content": "q1"}));
    assert_eq!(input[1], json!({"role": "assistant", "content": "A1"}));
    assert_eq!(input[2]["content"][0]["text"], "q2");

    let r = app
        .call(U, Method::GET, &format!("{CHATS}/{chat}/messages"), None)
        .await;
    let items = r.json["items"].as_array().unwrap();
    let got: Vec<(&str, &str)> = items
        .iter()
        .map(|m| (m["role"].as_str().unwrap(), m["content"].as_str().unwrap()))
        .collect();
    assert_eq!(
        got,
        [
            ("user", "q1"),
            ("assistant", "A1"),
            ("user", "q2"),
            ("assistant", "A2")
        ]
    );
}

#[tokio::test]
async fn no_buffering_first_delta_before_provider_finishes() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    // Events: created(0), delta "one"(1), delta "two"(2), completed(3); hold before 2.
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(2),
        ..ScriptedStream::text(&["one", "two"], 10, 5)
    });
    let (events, conn) = app
        .stream_until(U, &stream_path(chat), json!({"content": "hi"}), 2)
        .await;
    assert_eq!(events[0].0, "stream_started");
    assert_eq!(
        events[1],
        (
            "delta".to_owned(),
            json!({"type": "text", "content": "one"})
        )
    );
    app.provider.release();
    let rest = conn.rest().await;
    let names: Vec<&str> = rest.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["delta", "done"]);
}

#[tokio::test]
async fn ping_before_first_content() {
    let app = TestApp::builder()
        .config(|c| c.streaming.sse_ping_interval_seconds = 5)
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(1),
        ..ScriptedStream::text(&["a", "b"], 10, 5)
    });
    let (events, mut conn) = app
        .stream_until(U, &stream_path(chat), json!({"content": "hi"}), 1)
        .await;
    assert_eq!(events[0].0, "stream_started");
    let ping = conn.next_event().await.unwrap();
    assert_eq!(ping, ("ping".to_owned(), json!({})));
    app.provider.release();
    let rest = conn.rest().await;
    let first_delta = rest.iter().position(|(n, _)| n == "delta").unwrap();
    assert!(
        rest[first_delta..].iter().all(|(n, _)| n != "ping"),
        "{rest:?}"
    );
    assert_eq!(rest.last().unwrap().0, "done");
}

// ---------------------------------------------------------------------------------------------
// tools and citations
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn tool_and_citation_events() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream::events(vec![
        created(),
        web_search_start(1),
        web_search_done(1),
        delta_event("Hello world"),
        ev(
            "response.output_text.annotation.added",
            json!({
                "item_id": FAKE_ITEM_ID, "output_index": 1, "content_index": 0,
                "annotation_index": 0,
                "annotation": {
                    "type": "url_citation", "url": "https://example.com/a",
                    "title": "Example", "start_index": 0, "end_index": 5,
                },
            }),
        ),
        completed_event("Hello world", 20, 7),
    ]));
    let c = send(
        &app,
        chat,
        json!({"content": "search it", "web_search": {"enabled": true}}),
    )
    .await;
    assert_eq!(
        c.names(),
        [
            "stream_started",
            "tool",
            "tool",
            "delta",
            "citations",
            "done"
        ],
        "{c:?}"
    );
    assert_eq!(
        c.events[1].1,
        json!({"phase": "start", "name": "web_search", "details": {}})
    );
    assert_eq!(
        c.events[2].1,
        json!({"phase": "done", "name": "web_search", "details": {}})
    );
    assert_eq!(
        c.events[4].1,
        json!({"items": [{
            "source": "web", "url": "https://example.com/a", "title": "Example",
            "snippet": "Hello", "span": {"start": 0, "end": 5},
        }]})
    );

    let req = &app.provider.chat_requests()[0];
    assert_eq!(
        req["tools"],
        json!([{"type": "web_search", "search_context_size": "low"}])
    );
    assert_eq!(req["metadata"]["feature"], "web_search");
    assert_eq!(req["max_tool_calls"], 2);

    let request_id = uuid_of(&c.first("stream_started").unwrap()["request_id"]);
    let turn = turn_of(&app, chat, request_id).await;
    assert!(turn.web_search_enabled);
    assert_eq!(turn.web_search_completed_count, 1);
    let rows = quota_rows(&app, U).await;
    let daily_total = rows
        .iter()
        .find(|r| r.bucket == "total" && r.period_type == PeriodType::Daily.as_str())
        .unwrap();
    assert_eq!(daily_total.web_search_calls, 1);
}

#[tokio::test]
async fn web_search_calls_exceeded_mid_turn() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream::events(vec![
        created(),
        web_search_start(1),
        web_search_done(1),
        web_search_start(2),
        web_search_done(2),
        web_search_start(3),
        web_search_done(3),
        delta_event("late"),
        completed_event("late", 10, 5),
    ]));
    let c = send(
        &app,
        chat,
        json!({"content": "search", "web_search": {"enabled": true}}),
    )
    .await;
    let (name, data) = c.last().unwrap();
    assert_eq!(name, "error", "{c:?}");
    assert_eq!(data["code"], "web_search_calls_exceeded");
    assert!(c.first("delta").is_none(), "{c:?}");
    let request_id = uuid_of(&c.first("stream_started").unwrap()["request_id"]);
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("web_search_calls_exceeded")
    );
    let usage = app.outbox_messages_n(QueueKind::Usage, 1).await;
    assert_eq!(usage[0]["billing_outcome"], "failed");
    assert_eq!(usage[0]["settlement_method"], "estimated");
    eventually("provider stream released", || async {
        app.provider.open_streams() == 0
    })
    .await;
}

#[tokio::test]
async fn code_interpreter_calls_exceeded_mid_turn() {
    let app = TestApp::builder()
        .config(|c| c.quota.code_interpreter_max_calls_per_message = 1)
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    let start = || {
        ev(
            "response.code_interpreter_call.in_progress",
            json!({"item_id": "ci_1", "output_index": 0}),
        )
    };
    app.provider.push_stream(ScriptedStream::events(vec![
        created(),
        start(),
        start(),
        completed_event("never", 10, 5),
    ]));
    let c = send(&app, chat, json!({"content": "compute"})).await;
    assert_eq!(c.names(), ["stream_started", "tool", "error"], "{c:?}");
    assert_eq!(c.events[2].1["code"], "code_interpreter_calls_exceeded");
    let request_id = uuid_of(&c.events[0].1["request_id"]);
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(
        turn.error_code.as_deref(),
        Some("code_interpreter_calls_exceeded")
    );
}

#[tokio::test]
async fn incomplete_response_is_done_without_citations() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream::events(vec![
        created(),
        delta_event("Hello world"),
        ev(
            "response.output_text.annotation.added",
            json!({
                "item_id": FAKE_ITEM_ID, "output_index": 0, "content_index": 0,
                "annotation": {
                    "type": "url_citation", "url": "https://example.com/a",
                    "title": "Example", "start_index": 0, "end_index": 5,
                },
            }),
        ),
        ev(
            "response.incomplete",
            json!({"response": {
                "id": "resp_fake0001", "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 4, "output_tokens": 2},
            }}),
        ),
    ]));
    let c = send(&app, chat, json!({"content": "x"})).await;
    assert_eq!(c.names(), ["stream_started", "delta", "done"], "{c:?}");
    assert_eq!(
        c.events[2].1["usage"],
        json!({"input_tokens": 4, "output_tokens": 2})
    );
    let request_id = uuid_of(&c.events[0].1["request_id"]);
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "completed");
    assert!(turn.error_code.is_none());
}

async fn seed_vector_store(app: &TestApp, chat: Uuid, vs: &str) {
    let tenant_id = chat_row(app, chat).await.tenant_id;
    let row = chat_vector_store::Model {
        id: Uuid::new_v4(),
        tenant_id,
        chat_id: chat,
        vector_store_id: Some(vs.to_owned()),
        provider: "openai".to_owned(),
        file_count: 1,
        created_at: Utc::now(),
    };
    let conn = app.db.conn().unwrap();
    secure_insert::<chat_vector_store::Entity>(
        row.into_active_model(),
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn file_search_tool_and_file_citations_from_chat_attachments() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let doc = seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, U.user_id, "report.pdf"),
    )
    .await;

    // A ready document without a vector store: no file_search.
    let c = send(&app, chat, json!({"content": "one"})).await;
    assert_eq!(c.status, StatusCode::OK);
    assert!(app.provider.chat_requests()[0].get("tools").is_none());

    seed_vector_store(&app, chat, "vs_abcdefghijklmnop").await;
    let file_citation = |file_id: &str| {
        ev(
            "response.output_text.annotation.added",
            json!({
                "item_id": FAKE_ITEM_ID, "output_index": 0, "content_index": 0,
                "annotation": {"type": "file_citation", "file_id": file_id, "filename": "x", "index": 3},
            }),
        )
    };
    app.provider.push_stream(ScriptedStream::events(vec![
        created(),
        ev(
            "response.file_search_call.searching",
            json!({"item_id": "fs_1"}),
        ),
        ev(
            "response.file_search_call.completed",
            json!({"item_id": "fs_1"}),
        ),
        delta_event("From the report."),
        file_citation("file-seed"),
        file_citation("file-unknownabcdefgh"),
        completed_event("From the report.", 10, 5),
    ]));
    let c = send(&app, chat, json!({"content": "two"})).await;
    assert_eq!(
        c.names(),
        [
            "stream_started",
            "tool",
            "tool",
            "delta",
            "citations",
            "done"
        ],
        "{c:?}"
    );
    assert_eq!(
        c.events[2].1,
        json!({"phase": "done", "name": "file_search", "details": {"files_searched": 0}})
    );
    assert_eq!(
        c.events[4].1,
        json!({"items": [{
            "source": "file", "title": "report.pdf",
            "attachment_id": doc.to_string(), "snippet": "",
        }]})
    );
    let wire = serde_json::to_string(&c.events).unwrap();
    assert!(
        !wire.contains("file-seed") && !wire.contains("vs_"),
        "{wire}"
    );

    let req = &app.provider.chat_requests()[1];
    assert_eq!(
        req["tools"],
        json!([{"type": "file_search", "vector_store_ids": ["vs_abcdefghijklmnop"], "max_num_results": 5}])
    );
    assert_eq!(req["metadata"]["feature"], "file_search");
    assert!(
        req["instructions"]
            .as_str()
            .unwrap()
            .contains(&app.config.context.file_search_guard),
        "{req}"
    );
    let request_id = uuid_of(&c.events[0].1["request_id"]);
    assert_eq!(
        turn_of(&app, chat, request_id)
            .await
            .file_search_completed_count,
        1
    );

    // file_search kill switch: no tool even with documents and a vector store.
    let app2 = TestApp::builder()
        .kill_switches(KillSwitches {
            disable_file_search: true,
            ..no_kill_switches()
        })
        .build()
        .await;
    let chat2 = create_chat(&app2, U).await;
    seed::insert_attachment(
        &app2.db,
        NewAttachment::document(chat2, U.user_id, "report.pdf"),
    )
    .await;
    seed_vector_store(&app2, chat2, "vs_abcdefghijklmnop").await;
    assert_eq!(
        app2.stream(U, &stream_path(chat2), json!({"content": "x"}))
            .await
            .status,
        StatusCode::OK
    );
    assert!(app2.provider.chat_requests()[0].get("tools").is_none());
}

#[tokio::test]
async fn web_search_kill_switch_400_feature_disabled() {
    let app = TestApp::builder()
        .kill_switches(KillSwitches {
            disable_web_search: true,
            ..no_kill_switches()
        })
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    let c = send(
        &app,
        chat,
        json!({"content": "x", "web_search": {"enabled": true}}),
    )
    .await;
    let p = assert_problem(&c, StatusCode::BAD_REQUEST);
    let v = &p["context"]["violations"][0];
    assert_eq!(v["subject"], "web_search", "{p}");
    assert_eq!(v["type"], "FEATURE_DISABLED", "{p}");
    assert_nothing_written(&app, chat).await;

    // Without the flag the send proceeds (no web search tool).
    let c = send(&app, chat, json!({"content": "x"})).await;
    assert_eq!(c.status, StatusCode::OK);
}

#[tokio::test]
async fn web_search_daily_quota_429_web_search() {
    let app = TestApp::builder()
        .config(|c| c.quota.web_search_daily_quota = 1)
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream::events(vec![
        created(),
        web_search_start(1),
        web_search_done(1),
        delta_event("ok"),
        completed_event("ok", 10, 5),
    ]));
    let body = json!({"content": "search", "web_search": {"enabled": true}});
    assert_eq!(send(&app, chat, body.clone()).await.status, StatusCode::OK);

    let c = send(&app, chat, body).await;
    let p = assert_problem(&c, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        p["context"]["violations"][0]["subject"], "web_search",
        "{p}"
    );
    assert_eq!(app.provider.chat_requests().len(), 1);
    // A send without web search is not limited by the web search quota.
    assert_eq!(
        send(&app, chat, json!({"content": "plain"})).await.status,
        StatusCode::OK
    );
}

// ---------------------------------------------------------------------------------------------
// preflight validation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn preflight_validation_before_provider_call() {
    // Empty / whitespace content.
    {
        let app = app().await;
        let chat = create_chat(&app, U).await;
        for content in ["", "   \n"] {
            let c = send(&app, chat, json!({"content": content})).await;
            let p = assert_problem(&c, StatusCode::BAD_REQUEST);
            assert_eq!(field_reason(p), ("content", "EMPTY_CONTENT"), "{p}");
        }
        assert_nothing_written(&app, chat).await;
    }
    // Duplicate, foreign, not-ready, deleted and unknown attachment ids.
    {
        let app = app().await;
        let chat = create_chat(&app, U).await;
        let other = create_chat(&app, U).await;
        let doc =
            seed::insert_attachment(&app.db, NewAttachment::document(chat, U.user_id, "a.pdf"))
                .await;
        let foreign =
            seed::insert_attachment(&app.db, NewAttachment::document(other, U.user_id, "b.pdf"))
                .await;
        let not_ready = seed::insert_attachment(
            &app.db,
            NewAttachment::document(chat, U.user_id, "c.pdf").status("uploaded"),
        )
        .await;
        let deleted = seed::insert_attachment(
            &app.db,
            NewAttachment::document(chat, U.user_id, "d.pdf").deleted(Utc::now()),
        )
        .await;
        let other_uploader = seed::insert_attachment(
            &app.db,
            NewAttachment::document(chat, TestUser::A2.user_id, "e.pdf"),
        )
        .await;
        for ids in [
            vec![doc, doc],
            vec![doc, foreign],
            vec![not_ready],
            vec![deleted],
            vec![other_uploader],
            vec![Uuid::new_v4()],
        ] {
            let c = send(&app, chat, json!({"content": "x", "attachment_ids": ids})).await;
            let p = assert_problem(&c, StatusCode::BAD_REQUEST);
            assert_eq!(field_reason(p), ("attachment", "invalid_attachment"), "{p}");
            assert_eq!(
                p["context"]["resource_type"],
                "gts.cf.core.mini_chat.attachment.v1~"
            );
        }
        assert!(app.provider.chat_requests().is_empty());
        assert!(turns(&app, chat).await.is_empty());
        assert!(messages(&app, chat).await.is_empty());
        assert!(quota_rows(&app, U).await.is_empty());
    }
    // More than max_images_per_message images.
    {
        let app = app().await;
        let chat = create_chat(&app, U).await;
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(
                seed::insert_attachment(
                    &app.db,
                    NewAttachment::image(chat, U.user_id, &format!("{i}.png"), None),
                )
                .await,
            );
        }
        let c = send(&app, chat, json!({"content": "x", "attachment_ids": ids})).await;
        let p = assert_problem(&c, StatusCode::BAD_REQUEST);
        assert_eq!(field_reason(p), ("image_count", "TOO_MANY_IMAGES"), "{p}");
        assert_nothing_written(&app, chat).await;
    }
    // Image while images are disabled.
    {
        let app = TestApp::builder()
            .kill_switches(KillSwitches {
                disable_images: true,
                ..no_kill_switches()
            })
            .build()
            .await;
        let chat = create_chat(&app, U).await;
        let img = seed::insert_attachment(
            &app.db,
            NewAttachment::image(chat, U.user_id, "i.png", None),
        )
        .await;
        let c = send(&app, chat, json!({"content": "x", "attachment_ids": [img]})).await;
        let p = assert_problem(&c, StatusCode::BAD_REQUEST);
        let v = &p["context"]["violations"][0];
        assert_eq!(v["subject"], "images", "{p}");
        assert_eq!(v["type"], "FEATURE_DISABLED", "{p}");
        assert_nothing_written(&app, chat).await;
    }
    // Image on an effective model without vision (premium exhausted -> standard).
    {
        let app = premium_exhausted_app().await;
        assert!(
            !catalog::standard_model("x")
                .multimodal_capabilities
                .contains(&VISION_INPUT.to_owned())
        );
        let chat = create_chat(&app, U).await;
        let img = seed::insert_attachment(
            &app.db,
            NewAttachment::image(chat, U.user_id, "i.png", None),
        )
        .await;
        let c = send(&app, chat, json!({"content": "x", "attachment_ids": [img]})).await;
        let p = assert_problem(&c, StatusCode::BAD_REQUEST);
        assert_eq!(
            field_reason(p),
            ("content_type", "VISION_NOT_SUPPORTED"),
            "{p}"
        );
        assert_nothing_written(&app, chat).await;
    }
    // Message larger than max_input_tokens.
    {
        let mut models = catalog::default_catalog();
        models[0].max_input_tokens = 50;
        let app = TestApp::builder().catalog(models).build().await;
        let chat = create_chat(&app, U).await;
        let c = send(&app, chat, json!({"content": "long ".repeat(200)})).await;
        let p = assert_problem(&c, StatusCode::BAD_REQUEST);
        assert_eq!(field_reason(p), ("content", "INPUT_TOO_LONG"), "{p}");
        assert_nothing_written(&app, chat).await;
    }
    // Mandatory context larger than the window.
    {
        let mut models = catalog::default_catalog();
        models[0].context_window = 200;
        let app = TestApp::builder().catalog(models).build().await;
        let chat = create_chat(&app, U).await;
        let c = send(&app, chat, json!({"content": "hello"})).await;
        let p = assert_problem(&c, StatusCode::BAD_REQUEST);
        assert_eq!(
            field_reason(p),
            ("content", "CONTEXT_BUDGET_EXCEEDED"),
            "{p}"
        );
        assert_nothing_written(&app, chat).await;
    }
}

#[tokio::test]
async fn image_attachment_sent_as_input_image_and_linked() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let img = seed::insert_attachment(
        &app.db,
        NewAttachment::image(chat, U.user_id, "i.png", None),
    )
    .await;
    let c = send(
        &app,
        chat,
        json!({"content": "look", "attachment_ids": [img]}),
    )
    .await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    let req = &app.provider.chat_requests()[0];
    let last = req["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["content"],
        json!([
            {"type": "input_text", "text": "look"},
            {"type": "input_image", "file_id": "file-seed"},
        ])
    );
    let r = app
        .call(U, Method::GET, &format!("{CHATS}/{chat}/messages"), None)
        .await;
    assert_eq!(
        r.json["items"][0]["attachments"][0]["attachment_id"],
        img.to_string(),
        "{}",
        r.json
    );
}

#[tokio::test]
async fn send_on_removed_model_is_invalid_model() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let conn = app.db.conn().unwrap();
    chat::Entity::update_many()
        .col_expr(chat::Column::Model, Expr::value("removed-model"))
        .filter(chat::Column::Id.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    let c = send(&app, chat, json!({"content": "x"})).await;
    let p = assert_problem(&c, StatusCode::BAD_REQUEST);
    assert_eq!(field_reason(p), ("model", "INVALID_MODEL"), "{p}");
    assert_nothing_written(&app, chat).await;
}

#[tokio::test]
async fn quota_exhausted_429_tokens_no_provider_call_no_rows() {
    let app = TestApp::builder().limits(tiny(), tiny()).build().await;
    let chat = create_chat(&app, U).await;
    let c = send(&app, chat, json!({"content": "x"})).await;
    let p = assert_problem(&c, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(p["context"]["violations"][0]["subject"], "tokens", "{p}");
    assert_nothing_written(&app, chat).await;
}

#[tokio::test]
async fn downgrade_reported_in_done() {
    midnight_safe(downgrade_reported_in_done_body).await;
}

async fn downgrade_reported_in_done_body() {
    let app = premium_exhausted_app().await;
    let chat = create_chat(&app, U).await;
    // The premium daily budget is fully spent.
    let (day, _) = period_starts(now_utc());
    seed::insert_quota_row(
        &app.db,
        U.tenant_id,
        U.user_id,
        "tier:premium",
        PeriodType::Daily,
        day,
        tiny().limit_daily_credits_micro,
        0,
    )
    .await;
    let c = send(&app, chat, json!({"content": "x"})).await;
    let done = c.first("done").expect("done").clone();
    assert_eq!(done["effective_model"], "gpt-standard");
    assert_eq!(done["selected_model"], "gpt-premium");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_from"], "gpt-premium");
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");
    let msgs = messages(&app, chat).await;
    assert_eq!(msgs[1].model.as_deref(), Some("gpt-standard"));
    assert_eq!(app.provider.chat_requests()[0]["model"], "gpt-standard");
    // The exhausted premium tier is reported.
    let warnings = done["quota_warnings"].as_array().unwrap();
    let premium_daily = warnings
        .iter()
        .find(|w| w["tier"] == "premium" && w["period"] == "daily")
        .unwrap();
    assert_eq!(premium_daily["exhausted"], true, "{premium_daily}");
    assert!(premium_daily["next_reset"].is_string(), "{premium_daily}");
}

// ---------------------------------------------------------------------------------------------
// idempotency, replay and the parallel turn guard
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn replay_completed_request_id_is_side_effect_free() {
    let app = premium_exhausted_app().await;
    let chat = create_chat(&app, U).await;
    app.provider
        .push_stream(ScriptedStream::text(&["Hel", "lo"], 10, 5));
    let request_id = Uuid::new_v4();
    let body = json!({"content": "hi", "request_id": request_id});
    let first = send(&app, chat, body.clone()).await;
    let message_id = uuid_of(&first.first("stream_started").unwrap()["message_id"]);
    assert_eq!(app.outbox_messages_n(QueueKind::Usage, 1).await.len(), 1);
    assert_eq!(app.outbox_messages_n(QueueKind::Audit, 1).await.len(), 1);
    let quota_before = quota_rows(&app, U).await;

    let c = send(&app, chat, body).await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    assert_eq!(c.names(), ["stream_started", "delta", "done"]);
    let started = &c.events[0].1;
    assert_eq!(started["is_new_turn"], false);
    assert_eq!(uuid_of(&started["request_id"]), request_id);
    assert_eq!(uuid_of(&started["message_id"]), message_id);
    assert_eq!(c.events[1].1, json!({"type": "text", "content": "Hello"}));
    let done = &c.events[2].1;
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 10, "output_tokens": 5})
    );
    assert_eq!(done["effective_model"], "gpt-standard");
    assert_eq!(done["selected_model"], "gpt-premium");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_from"], "gpt-premium");
    assert!(done.get("downgrade_reason").is_none(), "{done}");
    assert!(done.get("quota_warnings").is_none(), "{done}");

    assert_eq!(app.provider.chat_requests().len(), 1);
    assert_eq!(quota_rows(&app, U).await, quota_before);
    assert_eq!(messages(&app, chat).await.len(), 2);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(app.outbox_messages(QueueKind::Usage).await.len(), 1);
    assert_eq!(app.outbox_messages(QueueKind::Audit).await.len(), 1);
}

#[tokio::test]
async fn replay_does_not_depend_on_the_chat_model_resolving() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let request_id = Uuid::new_v4();
    let body = json!({"content": "hi", "request_id": request_id});
    let first = send(&app, chat, body.clone()).await;
    assert_eq!(first.last().unwrap().0, "done", "{:?}", first.names());

    // The chat's model is no longer in the catalog.
    let conn = app.db.conn().unwrap();
    chat::Entity::update_many()
        .col_expr(chat::Column::Model, Expr::value("removed-model"))
        .filter(chat::Column::Id.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    let c = send(&app, chat, body).await;
    assert_eq!(c.status, StatusCode::OK, "{:?}", c.problem);
    assert_eq!(c.names(), ["stream_started", "delta", "done"]);
    assert_eq!(c.events[0].1["is_new_turn"], false);
    assert_eq!(app.provider.chat_requests().len(), 1);

    // A new turn still reports the unresolvable model.
    let c = send(&app, chat, json!({"content": "new"})).await;
    let p = assert_problem(&c, StatusCode::BAD_REQUEST);
    assert_eq!(field_reason(p), ("model", "INVALID_MODEL"), "{p}");
}

#[tokio::test]
async fn replay_checked_before_parallel_guard() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let first_id = Uuid::new_v4();
    let body = json!({"content": "one", "request_id": first_id});
    assert_eq!(send(&app, chat, body.clone()).await.status, StatusCode::OK);

    app.provider.push_stream(ScriptedStream {
        hold_after: Some(1),
        ..ScriptedStream::text(&["two"], 10, 5)
    });
    let (_, conn) = app
        .stream_until(U, &stream_path(chat), json!({"content": "two"}), 1)
        .await;

    let c = send(&app, chat, body).await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    assert_eq!(c.first("stream_started").unwrap()["is_new_turn"], false);

    app.provider.release();
    assert_eq!(conn.rest().await.last().unwrap().0, "done");
}

#[tokio::test]
async fn request_id_conflict_for_failed_or_running_or_deleted_turn() {
    let app = app().await;
    let conflict = |c: &SseCapture| {
        let p = assert_problem(c, StatusCode::CONFLICT);
        assert_eq!(p["context"]["reason"], "request_id_conflict", "{p}");
    };
    // failed
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream::failed("boom"));
    let failed_id = Uuid::new_v4();
    let body = json!({"content": "x", "request_id": failed_id});
    let c = send(&app, chat, body.clone()).await;
    assert_eq!(c.last().unwrap().0, "error");
    conflict(&send(&app, chat, body).await);

    // running
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(1),
        ..ScriptedStream::text(&["x"], 10, 5)
    });
    let running_id = Uuid::new_v4();
    let body = json!({"content": "x", "request_id": running_id});
    let (_, conn) = app
        .stream_until(U, &stream_path(chat), body.clone(), 1)
        .await;
    conflict(&send(&app, chat, body).await);
    app.provider.release();
    conn.rest().await;

    // completed but soft-deleted
    let chat = create_chat(&app, U).await;
    let deleted_id = Uuid::new_v4();
    let body = json!({"content": "x", "request_id": deleted_id});
    assert_eq!(send(&app, chat, body.clone()).await.status, StatusCode::OK);
    let turn = turn_of(&app, chat, deleted_id).await;
    let conn = app.db.conn().unwrap();
    assert!(
        TurnRepo::soft_delete(&conn, turn.id, None, Utc::now())
            .await
            .unwrap()
    );
    conflict(&send(&app, chat, body).await);
}

#[tokio::test]
async fn concurrent_sends_one_wins_other_409() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(1),
        ..ScriptedStream::text(&["x"], 10, 5)
    });
    let (_, conn) = app
        .stream_until(U, &stream_path(chat), json!({"content": "first"}), 1)
        .await;
    let c = send(&app, chat, json!({"content": "second"})).await;
    let p = assert_problem(&c, StatusCode::CONFLICT);
    assert_eq!(p["context"]["reason"], "turn_already_running", "{p}");
    assert_eq!(app.provider.chat_requests().len(), 1);
    app.provider.release();
    assert_eq!(conn.rest().await.last().unwrap().0, "done");
    // Only the first turn and its messages exist.
    assert_eq!(turns(&app, chat).await.len(), 1);
    assert_eq!(messages(&app, chat).await.len(), 2);
}

#[tokio::test]
async fn new_turn_accepted_after_previous_terminal() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider
        .push_stream(ScriptedStream::failed("first fails"));
    let c = send(&app, chat, json!({"content": "one"})).await;
    assert_eq!(c.last().unwrap().0, "error");
    let c = send(&app, chat, json!({"content": "two"})).await;
    assert_eq!(c.last().unwrap().0, "done", "{c:?}");
    assert_eq!(turns(&app, chat).await.len(), 2);
}

// ---------------------------------------------------------------------------------------------
// provider failures
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn provider_error_message_is_sanitized_in_sse() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream::failed(
        "bad file-abcdefghijklmnop at https://x.y resp_123",
    ));
    let c = send(&app, chat, json!({"content": "x"})).await;
    assert_eq!(c.names(), ["stream_started", "error"], "{c:?}");
    let err = &c.events[1].1;
    assert_eq!(err["code"], "provider_error");
    let msg = err["message"].as_str().unwrap();
    for leaked in ["file-", "https://", "resp_"] {
        assert!(!msg.contains(leaked), "{msg}");
    }
    let request_id = uuid_of(&c.events[0].1["request_id"]);
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert!(turn.assistant_message_id.is_none());
    assert_eq!(messages(&app, chat).await.len(), 1);
    let usage = app.outbox_messages_n(QueueKind::Usage, 1).await;
    assert_eq!(usage[0]["billing_outcome"], "failed");
    assert_eq!(usage[0]["settlement_method"], "estimated");
}

#[tokio::test]
async fn provider_429_rate_limited() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream::rate_limited(7));
    let c = send(&app, chat, json!({"content": "x"})).await;
    let (name, err) = c.last().unwrap();
    assert_eq!(name, "error", "{c:?}");
    assert_eq!(err["code"], "rate_limited");
    assert!(
        err["message"].as_str().unwrap().contains("retry in 7s"),
        "{err}"
    );
    let request_id = uuid_of(&c.events[0].1["request_id"]);
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.error_code.as_deref(), Some("rate_limited"));
}

#[tokio::test]
async fn provider_timeout() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream::gateway_timeout());
    let c = send(&app, chat, json!({"content": "x"})).await;
    let (name, err) = c.last().unwrap();
    assert_eq!(name, "error", "{c:?}");
    assert_eq!(err["code"], "provider_timeout");
    let request_id = uuid_of(&c.events[0].1["request_id"]);
    assert_eq!(
        turn_of(&app, chat, request_id).await.error_code.as_deref(),
        Some("provider_timeout")
    );
}

#[tokio::test]
async fn stream_without_terminal_is_provider_error() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider
        .push_stream(ScriptedStream::no_terminal(&["partial"]));
    let c = send(&app, chat, json!({"content": "x"})).await;
    assert_eq!(c.names(), ["stream_started", "delta", "error"], "{c:?}");
    assert_eq!(c.events[2].1["code"], "provider_error");
}

// ---------------------------------------------------------------------------------------------
// disconnects and CAS races
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stream_disconnect_mid_stream_cancels_turn_and_settles_once() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    // created(0), delta "partial "(1), delta "rest"(2): hold before 2.
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(2),
        ..ScriptedStream::text(&["partial ", "rest"], 10, 5)
    });
    let (events, conn) = app
        .stream_until(U, &stream_path(chat), json!({"content": "x"}), 2)
        .await;
    assert_eq!(events[1].1["content"], "partial ");
    let request_id = uuid_of(&events[0].1["request_id"]);
    let message_id = uuid_of(&events[0].1["message_id"]);
    drop(conn);

    let turn = wait_terminal(&app, chat, request_id).await;
    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.assistant_message_id, Some(message_id));
    let msgs = messages(&app, chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1].id, message_id);
    assert_eq!(msgs[1].content, "partial ");
    eventually("provider stream released", || async {
        app.provider.open_streams() == 0
    })
    .await;

    let usage = app.outbox_messages_n(QueueKind::Usage, 1).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["billing_outcome"], "aborted");
    assert_eq!(usage[0]["settlement_method"], "estimated");
    assert_eq!(usage[0]["terminal_state"], "cancelled");
    let rows = quota_rows(&app, U).await;
    assert!(!rows.is_empty());
    for row in rows {
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
        assert_eq!(row.calls, 1, "{row:?}");
    }
    // Releasing the (already dropped) provider stream changes nothing.
    app.provider.release();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(app.outbox_messages(QueueKind::Usage).await.len(), 1);
}

#[tokio::test]
async fn stream_disconnect_during_setup_still_finalizes() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let request_id = Uuid::new_v4();
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(1),
        ..ScriptedStream::text(&["x"], 10, 5)
    });
    let req = mini_chat::testing::sse::json_request(
        U,
        Method::POST,
        &stream_path(chat),
        &json!({"content": "x", "request_id": request_id}),
    );
    {
        let fut = app.raw(req);
        tokio::pin!(fut);
        // One poll starts the handler (which spawns the setup), then the client goes away.
        assert!(futures::poll!(fut.as_mut()).is_pending());
    }
    let turn = wait_terminal(&app, chat, request_id).await;
    assert_eq!(turn.state, "cancelled");
    assert!(turn.assistant_message_id.is_none());
    let usage = app.outbox_messages_n(QueueKind::Usage, 1).await;
    assert_eq!(usage[0]["billing_outcome"], "aborted");
}

#[tokio::test]
async fn cancel_before_any_text_has_null_assistant_message() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(1),
        ..ScriptedStream::text(&["never"], 10, 5)
    });
    let (events, conn) = app
        .stream_until(U, &stream_path(chat), json!({"content": "x"}), 1)
        .await;
    let request_id = uuid_of(&events[0].1["request_id"]);
    drop(conn);
    let turn = wait_terminal(&app, chat, request_id).await;
    assert_eq!(turn.state, "cancelled");
    assert!(turn.assistant_message_id.is_none());
    assert_eq!(messages(&app, chat).await.len(), 1);
}

#[tokio::test]
async fn stream_interrupted_when_watchdog_wins() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(1),
        ..ScriptedStream::text(&["late"], 10, 5)
    });
    let (events, conn) = app
        .stream_until(U, &stream_path(chat), json!({"content": "x"}), 1)
        .await;
    let request_id = uuid_of(&events[0].1["request_id"]);
    let turn = turn_of(&app, chat, request_id).await;
    let db_conn = app.db.conn().unwrap();
    let won = TurnRepo::cas_finalize(
        &db_conn,
        turn.id,
        &TerminalUpdate {
            state: TurnState::Failed,
            error_code: Some("orphan_timeout".to_owned()),
            error_detail: None,
            assistant_message_id: None,
            provider_response_id: None,
            counters: TurnCounters::default(),
            now: Utc::now(),
        },
    )
    .await
    .unwrap();
    assert!(won);
    app.provider.release();
    let rest = conn.rest().await;
    let (name, data) = rest.last().unwrap();
    assert_eq!(name, "error", "{rest:?}");
    assert_eq!(data["code"], "stream_interrupted");
    assert!(rest.iter().all(|(n, _)| n != "done"), "{rest:?}");
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    // The CAS loser wrote nothing.
    assert_eq!(messages(&app, chat).await.len(), 1);
    app.assert_no_outbox(QueueKind::Usage, Duration::from_millis(300))
        .await;
}

// ---------------------------------------------------------------------------------------------
// turn status and authorization
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn turn_status_states() {
    let app = app().await;
    let chat = create_chat(&app, U).await;

    // running
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(1),
        ..ScriptedStream::text(&["done"], 10, 5)
    });
    let running_id = Uuid::new_v4();
    let (_, conn) = app
        .stream_until(
            U,
            &stream_path(chat),
            json!({"content": "x", "request_id": running_id}),
            1,
        )
        .await;
    let r = app
        .call(U, Method::GET, &turn_path(chat, running_id), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["state"], "running");
    assert_eq!(r.json["request_id"], running_id.to_string());
    assert!(r.json.get("assistant_message_id").is_none(), "{}", r.json);
    assert!(r.json.get("error_code").is_none(), "{}", r.json);
    assert!(r.json["updated_at"].is_string());

    // done
    app.provider.release();
    let rest = conn.rest().await;
    assert_eq!(rest.last().unwrap().0, "done");
    let r = app
        .call(U, Method::GET, &turn_path(chat, running_id), None)
        .await;
    assert_eq!(r.json["state"], "done", "{}", r.json);
    let assistant = messages(&app, chat).await[1].id;
    assert_eq!(r.json["assistant_message_id"], assistant.to_string());

    // error
    app.provider.push_stream(ScriptedStream::failed("nope"));
    let failed_id = Uuid::new_v4();
    send(&app, chat, json!({"content": "y", "request_id": failed_id})).await;
    let r = app
        .call(U, Method::GET, &turn_path(chat, failed_id), None)
        .await;
    assert_eq!(r.json["state"], "error", "{}", r.json);
    assert_eq!(r.json["error_code"], "provider_error");
    assert!(r.json.get("assistant_message_id").is_none(), "{}", r.json);

    // unknown request id
    let r = app
        .call(U, Method::GET, &turn_path(chat, Uuid::new_v4()), None)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    assert_eq!(
        r.json["context"]["resource_type"],
        "gts.cf.core.mini_chat.turn.v1~"
    );

    // soft-deleted turn
    let turn = turn_of(&app, chat, failed_id).await;
    let conn = app.db.conn().unwrap();
    TurnRepo::soft_delete(&conn, turn.id, None, Utc::now())
        .await
        .unwrap();
    let r = app
        .call(U, Method::GET, &turn_path(chat, failed_id), None)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);

    // another user's chat
    for user in [TestUser::A2, TestUser::B1] {
        let r = app
            .call(user, Method::GET, &turn_path(chat, running_id), None)
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    }
}

#[tokio::test]
async fn foreign_chat_send_404() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    for user in [TestUser::A2, TestUser::B1] {
        let c = app
            .stream(user, &stream_path(chat), json!({"content": "x"}))
            .await;
        let p = assert_problem(&c, StatusCode::NOT_FOUND);
        assert_eq!(
            p["context"]["resource_type"],
            "gts.cf.core.mini_chat.chat.v1~"
        );
    }
    let c = send(&app, Uuid::new_v4(), json!({"content": "x"})).await;
    assert_problem(&c, StatusCode::NOT_FOUND);
    assert!(app.provider.chat_requests().is_empty());
}

#[tokio::test]
async fn openapi_declares_stream_and_turn_status() {
    use toolkit::api::{OpenApiInfo, OpenApiRegistryImpl};

    let app = app().await;
    let registry = OpenApiRegistryImpl::new();
    let _router = mini_chat::api::rest::routes::register_routes(
        axum::Router::new(),
        &registry,
        app.services.clone(),
        &app.config,
    );
    let doc =
        serde_json::to_value(registry.build_openapi(&OpenApiInfo::default()).unwrap()).unwrap();
    let stream = &doc["paths"]["/mini-chat/v1/chats/{id}/messages:stream"]["post"];
    assert_eq!(
        stream["operationId"], "mini_chat.stream_message",
        "{stream}"
    );
    assert!(
        stream["responses"]["200"]["content"]["text/event-stream"].is_object(),
        "{stream}"
    );
    let turn = &doc["paths"]["/mini-chat/v1/chats/{id}/turns/{request_id}"]["get"];
    assert_eq!(turn["operationId"], "mini_chat.get_turn", "{turn}");
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["TurnStatusState"]["enum"],
        json!(["running", "done", "error", "cancelled"])
    );
    assert_eq!(schemas["DeltaKind"]["enum"], json!(["text", "reasoning"]));
    assert_eq!(schemas["ToolPhase"]["enum"], json!(["start", "done"]));
    assert_eq!(schemas["CitationSource"]["enum"], json!(["file", "web"]));
    assert_eq!(
        schemas["QuotaDecisionKind"]["enum"],
        json!(["allow", "downgrade"])
    );
}
