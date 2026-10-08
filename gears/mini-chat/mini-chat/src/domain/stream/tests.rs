//! Send pipeline tests through the HTTP router: SSE contract, persistence and settlement,
//! idempotent replay, the parallel-turn guard and live (unbuffered) delivery.

use std::time::Duration;

use mini_chat_sdk::credits_micro_checked;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use std::sync::Arc;

use tokio::sync::{Notify, mpsc};

use super::events::{DeltaKind, StreamEvent};
use super::relay;
use crate::domain::quota::{Bucket, Period};
use crate::test_support::app::{PREMIUM_LIMITS, SseFrame, SseReader, TestApp, ctx, test_config};
use crate::test_support::catalog::test_catalog;
use crate::test_support::gateway::{Responder, SseScript};
use crate::test_support::stream::{
    AUDIT_QUEUE, SeedAttachment, USAGE_QUEUE, annotation, answer, chat_row, completed, create_chat,
    event_names, failed, failed_with_usage, frame, function_call, incomplete, messages_of,
    provider_calls, provider_event, quota_row, quota_rows, script_provider,
    script_provider_sequence, seed_assistant_message, seed_attachment, seed_spent,
    seed_vector_store, set_turn_state, stream_uri, text_delta, turn_of, turns_of,
};

/// Premium credit multiplier of the test catalog (input and output).
const PREMIUM_MULT: i64 = 3_000_000;

struct Caller {
    tenant: Uuid,
    user: Uuid,
    who: SecurityContext,
}

fn new_user() -> Caller {
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    Caller {
        tenant,
        user,
        who: ctx(tenant, user),
    }
}

async fn send(app: &TestApp, u: &Caller, chat: Uuid, body: Value) -> Vec<SseFrame> {
    match app.stream("POST", &stream_uri(chat), &u.who, body).await {
        Ok(frames) => frames,
        Err(res) => panic!("send rejected: {} {}", res.status, res.json),
    }
}

fn uuid_of(v: &Value) -> Uuid {
    v.as_str().expect("uuid string").parse().expect("uuid")
}

async fn wait_for_payloads(app: &TestApp, queue: &str, n: usize) {
    TestApp::wait_until(&format!("{n} payload(s) on {queue}"), || async {
        app.outbox_payloads(queue).len() >= n
    })
    .await;
}

#[tokio::test]
async fn send_streams_contract_in_order() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["Hel", "lo"], 12, 5));
    let request_id = Uuid::new_v4();

    let reader = app
        .open_stream(
            "POST",
            &stream_uri(chat),
            &u.who,
            json!({"content": "hi", "request_id": request_id}),
        )
        .await
        .expect("stream opened");
    assert_eq!(reader.headers["content-type"], "text/event-stream");
    assert_eq!(reader.headers["x-accel-buffering"], "no");
    let frames = reader.collect().await;

    assert_eq!(
        event_names(&frames),
        ["stream_started", "delta", "delta", "done"]
    );
    let started = &frames[0].data;
    assert_eq!(uuid_of(&started["request_id"]), request_id);
    assert_eq!(started["is_new_turn"], true);
    uuid_of(&started["message_id"]);
    assert!(started.get("thread_summary_applied").is_none(), "{started}");
    assert_eq!(frames[1].data, json!({"type": "text", "content": "Hel"}));
    assert_eq!(frames[2].data, json!({"type": "text", "content": "lo"}));

    let done = &frames[3].data;
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 12, "output_tokens": 5})
    );
    assert_eq!(done["effective_model"], "gpt-premium");
    assert_eq!(done["selected_model"], "gpt-premium");
    assert_eq!(done["quota_decision"], "allow");
    for absent in [
        "downgrade_from",
        "downgrade_reason",
        "message_id",
        "request_id",
    ] {
        assert!(done.get(absent).is_none(), "{absent} in {done}");
    }
    // The provider's response id (`resp_1`) never reaches the client.
    let wire: String = frames.iter().map(|f| f.data.to_string()).collect();
    assert!(!wire.contains("resp_"), "{wire}");
}

#[tokio::test]
async fn send_persists_turn_messages_and_settlement() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    let created = chat_row(&app, chat).await;
    script_provider(&app, answer(&["Hel", "lo"], 12, 5));
    let request_id = Uuid::new_v4();

    let frames = send(
        &app,
        &u,
        chat,
        json!({"content": "hi", "request_id": request_id}),
    )
    .await;
    let message_id = uuid_of(&frame(&frames, "stream_started").data["message_id"]);

    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.assistant_message_id, Some(message_id));
    assert_eq!(turn.provider_response_id.as_deref(), Some("resp_1"));
    assert_eq!(turn.requester_type, "user");
    assert_eq!(turn.requester_user_id, Some(u.user));
    assert_eq!(turn.effective_model.as_deref(), Some("gpt-premium"));
    assert!(turn.error_code.is_none());
    assert!(turn.completed_at.is_some());

    let msgs = messages_of(&app, chat).await;
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    let (user_msg, assistant) = (&msgs[0], &msgs[1]);
    assert_eq!(
        (user_msg.role.as_str(), user_msg.content.as_str()),
        ("user", "hi")
    );
    assert_eq!(
        (assistant.role.as_str(), assistant.content.as_str()),
        ("assistant", "Hello")
    );
    assert_eq!(assistant.id, message_id);
    assert_eq!(user_msg.request_id, Some(request_id));
    assert_eq!(assistant.request_id, Some(request_id));
    assert_eq!(assistant.model.as_deref(), Some("gpt-premium"));
    assert_eq!((assistant.input_tokens, assistant.output_tokens), (12, 5));
    assert_eq!(assistant.provider_response_id.as_deref(), Some("resp_1"));
    assert!(chat_row(&app, chat).await.updated_at > created.updated_at);

    let expected = credits_micro_checked(12, 5, PREMIUM_MULT, PREMIUM_MULT).unwrap();
    for bucket in [Bucket::Total, Bucket::Premium] {
        let row = quota_row(&app, u.tenant, u.user, Period::Daily, bucket)
            .await
            .expect("daily row");
        assert_eq!(row.reserved_credits_micro, 0, "{bucket:?}");
        assert_eq!(row.spent_credits_micro, expected, "{bucket:?}");
        assert_eq!(row.calls, 1, "{bucket:?}");
    }

    wait_for_payloads(&app, USAGE_QUEUE, 1).await;
    wait_for_payloads(&app, AUDIT_QUEUE, 1).await;
    let usage = app.outbox_payloads(USAGE_QUEUE);
    assert_eq!(usage.len(), 1, "{usage:?}");
    let usage = &usage[0];
    assert_eq!(usage["billing_outcome"], "completed");
    assert_eq!(usage["settlement_method"], "actual");
    assert_eq!(usage["terminal_state"], "completed");
    assert_eq!(usage["requester_type"], "user");
    assert_eq!(usage["actual_credits_micro"], expected);
    assert_eq!(usage["usage"]["input_tokens"], 12);
    assert_eq!(
        usage["dedupe_key"],
        format!(
            "{}/{}/{}",
            u.tenant.simple(),
            turn.id.simple(),
            request_id.simple()
        )
    );
    let audit = app.outbox_payloads(AUDIT_QUEUE);
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["event_type"], "turn_completed");
    assert_eq!(audit[0]["policy_decisions"]["quota"]["decision"], "allow");
}

#[tokio::test]
async fn server_generates_request_id_when_omitted() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["ok"], 1, 1));

    let frames = send(&app, &u, chat, json!({"content": "hi"})).await;
    let request_id = uuid_of(&frame(&frames, "stream_started").data["request_id"]);
    assert_eq!(request_id.get_version_num(), 4);
    assert_eq!(turn_of(&app, chat, request_id).await.state, "completed");
}

#[tokio::test]
async fn provider_request_contains_history_and_message() {
    let mut catalog = test_catalog();
    catalog[0].system_prompt = "You are a careful assistant.".to_owned();
    let app = TestApp::builder().catalog(catalog).build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider_sequence(
        &app,
        vec![
            answer(&["first answer"], 3, 2),
            answer(&["second answer"], 3, 2),
        ],
    );

    send(&app, &u, chat, json!({"content": "first question"})).await;
    send(&app, &u, chat, json!({"content": "second question"})).await;

    let calls = provider_calls(&app);
    assert_eq!(calls.len(), 2);
    let second = &calls[1];
    assert_eq!(
        second["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "first question"}]},
            {"role": "assistant", "content": [{"type": "output_text", "text": "first answer"}]},
            {"role": "user", "content": [{"type": "input_text", "text": "second question"}]},
        ])
    );
    assert!(
        second["instructions"]
            .as_str()
            .unwrap()
            .starts_with("You are a careful assistant."),
        "{}",
        second["instructions"]
    );
    assert_eq!(second["model"], "gpt-premium");
    assert_eq!(
        second["user"],
        format!("{}{}", u.tenant.simple(), u.user.simple())
    );
    assert_eq!(second["metadata"]["request_type"], "chat");
    assert_eq!(second["metadata"]["feature"], "none");
}

#[tokio::test]
async fn downgrade_reported_in_done_and_message_model() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, Some("gpt-premium")).await;
    seed_spent(
        &app,
        u.tenant,
        u.user,
        Period::Daily,
        Bucket::Premium,
        PREMIUM_LIMITS.limit_daily_credits_micro,
    )
    .await;
    script_provider(&app, answer(&["ok"], 4, 2));

    let frames = send(&app, &u, chat, json!({"content": "hi"})).await;
    let done = &frame(&frames, "done").data;
    assert_eq!(done["effective_model"], "gpt-standard");
    assert_eq!(done["selected_model"], "gpt-premium");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_from"], "gpt-premium");
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");

    let assistant = messages_of(&app, chat).await.pop().unwrap();
    assert_eq!(assistant.model.as_deref(), Some("gpt-standard"));
    assert_eq!(provider_calls(&app)[0]["model"], "gpt-standard");
}

#[tokio::test]
async fn replay_is_side_effect_free() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["Hel", "lo"], 12, 5));
    let request_id = Uuid::new_v4();
    let body = json!({"content": "hi", "request_id": request_id});

    let original = send(&app, &u, chat, body.clone()).await;
    wait_for_payloads(&app, USAGE_QUEUE, 1).await;
    wait_for_payloads(&app, AUDIT_QUEUE, 1).await;
    let quota_before = quota_rows(&app, u.tenant, u.user).await;

    let replay = send(&app, &u, chat, body).await;
    assert_eq!(event_names(&replay), ["stream_started", "delta", "done"]);
    let started = &replay[0].data;
    assert_eq!(started["is_new_turn"], false);
    assert_eq!(uuid_of(&started["request_id"]), request_id);
    assert_eq!(
        started["message_id"],
        frame(&original, "stream_started").data["message_id"]
    );
    assert_eq!(replay[1].data, json!({"type": "text", "content": "Hello"}));
    let done = &replay[2].data;
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 12, "output_tokens": 5})
    );
    assert_eq!(done["effective_model"], "gpt-premium");
    assert_eq!(done["selected_model"], "gpt-premium");
    assert_eq!(done["quota_decision"], "allow");
    for absent in ["downgrade_from", "downgrade_reason", "quota_warnings"] {
        assert!(done.get(absent).is_none(), "{absent} in {done}");
    }

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(quota_rows(&app, u.tenant, u.user).await, quota_before);
    assert_eq!(app.outbox_payloads(USAGE_QUEUE).len(), 1);
    assert_eq!(app.outbox_payloads(AUDIT_QUEUE).len(), 1);
    assert_eq!(provider_calls(&app).len(), 1);
    assert_eq!(turns_of(&app, chat).await.len(), 1);
    assert_eq!(messages_of(&app, chat).await.len(), 2);
}

/// Replay needs nothing from the policy snapshot (DESIGN 4135, 4150): it still works when the
/// chat's model left the catalog and when the policy plugin fails.
#[tokio::test]
async fn replay_does_not_depend_on_catalog_or_policy_plugin() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["Hel", "lo"], 12, 5));
    let request_id = Uuid::new_v4();
    let body = json!({"content": "hi", "request_id": request_id});
    send(&app, &u, chat, body.clone()).await;

    // The chat's model (gpt-premium) is removed from the catalog.
    let without_model: Vec<_> = test_catalog()
        .into_iter()
        .filter(|m| m.id != "gpt-premium")
        .collect();
    app.usage.set_catalog(without_model);
    let replay = send(&app, &u, chat, body.clone()).await;
    assert_eq!(event_names(&replay), ["stream_started", "delta", "done"]);
    assert_eq!(replay[0].data["is_new_turn"], false);
    assert_eq!(replay[2].data["effective_model"], "gpt-premium");

    // The policy plugin is down.
    app.usage.set_catalog(test_catalog());
    app.usage.fail_snapshots(true);
    let replay = send(&app, &u, chat, body).await;
    assert_eq!(event_names(&replay), ["stream_started", "delta", "done"]);
    assert_eq!(replay[0].data["is_new_turn"], false);
    assert_eq!(provider_calls(&app).len(), 1);
}

#[tokio::test]
async fn replay_checked_before_parallel_guard() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider_sequence(
        &app,
        vec![
            answer(&["done"], 2, 1),
            Responder::Sse(vec![SseScript::Hang]),
        ],
    );
    let completed_id = Uuid::new_v4();
    send(
        &app,
        &u,
        chat,
        json!({"content": "one", "request_id": completed_id}),
    )
    .await;

    let mut running = app
        .open_stream("POST", &stream_uri(chat), &u.who, json!({"content": "two"}))
        .await
        .expect("second turn starts");
    let (started, _) = running.next_frame().await.expect("stream_started");
    assert_eq!(started.event, "stream_started");

    let replay = send(
        &app,
        &u,
        chat,
        json!({"content": "one", "request_id": completed_id}),
    )
    .await;
    assert_eq!(event_names(&replay), ["stream_started", "delta", "done"]);

    let rejected = app
        .open_stream(
            "POST",
            &stream_uri(chat),
            &u.who,
            json!({"content": "three"}),
        )
        .await
        .err()
        .expect("409 while a turn is running");
    assert_eq!(rejected.status, 409, "{}", rejected.json);
    assert_eq!(rejected.json["context"]["reason"], "turn_already_running");
    drop(running);
}

#[tokio::test]
async fn request_id_conflicts() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider_sequence(
        &app,
        vec![
            Responder::Sse(vec![failed("boom")]),
            answer(&["ok"], 1, 1),
            Responder::Sse(vec![SseScript::Hang]),
        ],
    );
    let conflict = |res: crate::test_support::app::TestResponse| {
        assert_eq!(res.status, 409, "{}", res.json);
        assert_eq!(res.json["context"]["reason"], "request_id_conflict");
    };

    // failed turn
    let failed_id = Uuid::new_v4();
    let frames = send(
        &app,
        &u,
        chat,
        json!({"content": "a", "request_id": failed_id}),
    )
    .await;
    assert_eq!(event_names(&frames), ["stream_started", "error"]);
    assert_eq!(turn_of(&app, chat, failed_id).await.state, "failed");
    conflict(
        app.call(
            "POST",
            &stream_uri(chat),
            &u.who,
            Some(json!({"content": "a", "request_id": failed_id})),
        )
        .await,
    );

    // soft-deleted completed turn
    let deleted_id = Uuid::new_v4();
    send(
        &app,
        &u,
        chat,
        json!({"content": "b", "request_id": deleted_id}),
    )
    .await;
    soft_delete_turn(&app, turn_of(&app, chat, deleted_id).await.id).await;
    conflict(
        app.call(
            "POST",
            &stream_uri(chat),
            &u.who,
            Some(json!({"content": "b", "request_id": deleted_id})),
        )
        .await,
    );

    // running turn
    let running_id = Uuid::new_v4();
    let mut running = app
        .open_stream(
            "POST",
            &stream_uri(chat),
            &u.who,
            json!({"content": "c", "request_id": running_id}),
        )
        .await
        .expect("running turn");
    running.next_frame().await.expect("stream_started");
    conflict(
        app.call(
            "POST",
            &stream_uri(chat),
            &u.who,
            Some(json!({"content": "c", "request_id": running_id})),
        )
        .await,
    );
    drop(running);
}

async fn soft_delete_turn(app: &TestApp, turn_id: Uuid) {
    use sea_orm::sea_query::Expr;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    use toolkit_db::secure::{AccessScope, SecureUpdateExt};

    use crate::infra::db::entity::chat_turns;
    use crate::infra::db::ts::db_now;

    let conn = app.db.conn().unwrap();
    chat_turns::Entity::update_many()
        .col_expr(chat_turns::Column::DeletedAt, Expr::value(db_now()))
        .filter(chat_turns::Column::Id.eq(turn_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_sends_one_wins_no_reserve_leak() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, Responder::Sse(vec![SseScript::Hang]));
    let uri = stream_uri(chat);

    let (first, second) = tokio::join!(
        app.open_stream("POST", &uri, &u.who, json!({"content": "a"})),
        app.open_stream("POST", &uri, &u.who, json!({"content": "b"})),
    );
    let (winner, loser) = match (first, second) {
        (Ok(opened), Err(rejected)) | (Err(rejected), Ok(opened)) => (opened, rejected),
        (Ok(_), Ok(_)) => panic!("both sends opened a stream"),
        (Err(one), Err(other)) => panic!("both sends rejected: {} / {}", one.json, other.json),
    };
    assert_eq!(loser.status, 409, "{}", loser.json);
    assert_eq!(loser.json["context"]["reason"], "turn_already_running");
    assert_eq!(turns_of(&app, chat).await.len(), 1);

    // The client disconnects: the turn is cancelled and its reserve settled.
    drop(winner);
    TestApp::wait_until("the turn is cancelled", || async {
        turns_of(&app, chat).await[0].state == "cancelled"
    })
    .await;
    let rows = quota_rows(&app, u.tenant, u.user).await;
    assert!(!rows.is_empty());
    for row in rows {
        assert_eq!(
            row.reserved_credits_micro, 0,
            "{} {}",
            row.period_type, row.bucket
        );
    }
}

#[tokio::test]
async fn deltas_are_not_buffered() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(
        &app,
        Responder::Sse(vec![
            text_delta("a"),
            SseScript::Delay(Duration::from_millis(800)),
            text_delta("b"),
            crate::test_support::stream::completed(2, 2),
        ]),
    );

    let mut reader = app
        .open_stream("POST", &stream_uri(chat), &u.who, json!({"content": "hi"}))
        .await
        .expect("stream opened");
    let (started, t0) = reader.next_frame().await.expect("stream_started");
    assert_eq!(started.event, "stream_started");
    let (first, t1) = reader.next_frame().await.expect("first delta");
    assert_eq!(first.data, json!({"type": "text", "content": "a"}));
    assert!(
        t1 - t0 < Duration::from_millis(500),
        "first delta after {:?}",
        t1 - t0
    );
    let (second, t2) = reader.next_frame().await.expect("second delta");
    assert_eq!(second.data["content"], "b");
    assert!(t2 - t1 >= Duration::from_millis(700), "{:?}", t2 - t1);
    let rest = reader.collect().await;
    assert_eq!(event_names(&rest), ["done"]);
}

// ---- Task 15: tools, citations, provider errors, cancellation, finalization outcomes ----------

async fn open(app: &TestApp, u: &Caller, chat: Uuid, body: Value) -> SseReader {
    match app
        .open_stream("POST", &stream_uri(chat), &u.who, body)
        .await
    {
        Ok(reader) => reader,
        Err(res) => panic!("send rejected: {} {}", res.status, res.json),
    }
}

/// Reads the next event and checks its name.
async fn expect_frame(reader: &mut SseReader, event: &str) -> SseFrame {
    let (frame, _) = tokio::time::timeout(Duration::from_secs(10), reader.next_frame())
        .await
        .expect("next SSE event in time")
        .unwrap_or_else(|| panic!("stream ended before `{event}`"));
    assert_eq!(frame.event, event, "{frame:?}");
    frame
}

/// The single usage payload of the turn.
async fn usage_payload(app: &TestApp) -> Value {
    wait_for_payloads(app, USAGE_QUEUE, 1).await;
    let usage = app.outbox_payloads(USAGE_QUEUE);
    assert_eq!(usage.len(), 1, "{usage:?}");
    usage[0].clone()
}

fn premium_credits(input_tokens: i64, output_tokens: i64) -> i64 {
    credits_micro_checked(input_tokens, output_tokens, PREMIUM_MULT, PREMIUM_MULT).unwrap()
}

/// Spent credits of the user's current daily `total` row.
async fn daily_spent(app: &TestApp, u: &Caller) -> i64 {
    quota_row(app, u.tenant, u.user, Period::Daily, Bucket::Total)
        .await
        .expect("daily total row")
        .spent_credits_micro
}

/// The estimated settlement of the turn: `credits(estimated_input, floor)` from its row.
fn estimated_credits(turn: &crate::infra::db::entity::chat_turns::Model) -> i64 {
    let reserve = turn.reserve_tokens.expect("reserve_tokens");
    let max_output = i64::from(turn.max_output_tokens_applied.expect("max_output"));
    let floor = i64::from(turn.minimal_generation_floor_applied.expect("floor"));
    premium_credits(reserve - max_output, floor)
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one contract scenario with its persisted effects
async fn tool_events_and_citations_contract() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    let doc = seed_attachment(&app, SeedAttachment::document(u.tenant, chat, u.user)).await;
    seed_vector_store(&app, u.tenant, chat).await;
    script_provider(
        &app,
        Responder::Sse(vec![
            provider_event("response.file_search_call.searching"),
            SseScript::event(
                "response.file_search_call.completed",
                json!({"type": "response.file_search_call.completed", "results": []}),
            ),
            provider_event("response.web_search_call.searching"),
            provider_event("response.web_search_call.completed"),
            text_delta("Rust is fast."),
            annotation(
                0,
                &json!({"type": "url_citation", "url": "https://rust-lang.org", "title": "Rust",
                       "start_index": 0, "end_index": 4}),
            ),
            annotation(
                1,
                &json!({"type": "file_citation", "file_id": format!("file-{}", doc.simple()),
                       "filename": "provider-name.pdf", "index": 3}),
            ),
            annotation(
                2,
                &json!({"type": "file_citation", "file_id": "file-UnknownFile0001",
                       "filename": "other.pdf", "index": 5}),
            ),
            completed(10, 4),
        ]),
    );
    let request_id = Uuid::new_v4();

    let frames = send(
        &app,
        &u,
        chat,
        json!({"content": "is rust fast?", "request_id": request_id,
               "web_search": {"enabled": true}}),
    )
    .await;

    assert!(
        provider_calls(&app)[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "file_search"),
        "file_search is part of the request"
    );
    assert_eq!(
        event_names(&frames),
        [
            "stream_started",
            "tool",
            "tool",
            "tool",
            "tool",
            "delta",
            "citations",
            "done"
        ]
    );
    assert_eq!(
        frames[1].data,
        json!({"phase": "start", "name": "file_search", "details": {}})
    );
    assert_eq!(
        frames[2].data,
        json!({"phase": "done", "name": "file_search", "details": {"files_searched": 0}})
    );
    assert_eq!(
        frames[3].data,
        json!({"phase": "start", "name": "web_search", "details": {}})
    );
    assert_eq!(
        frames[4].data,
        json!({"phase": "done", "name": "web_search", "details": {}})
    );
    assert_eq!(
        frame(&frames, "citations").data,
        json!({"items": [
            {"source": "web", "title": "Rust", "url": "https://rust-lang.org",
             "snippet": "Rust", "span": {"start": 0, "end": 4}},
            {"source": "file", "title": "report.pdf", "attachment_id": doc, "snippet": ""},
        ]})
    );
    let body: String = frames.iter().map(|f| f.data.to_string()).collect();
    assert!(!body.contains("file-"), "{body}");

    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.web_search_completed_count, 1);
    assert_eq!(turn.file_search_completed_count, 1);
    assert_eq!(turn.code_interpreter_completed_count, 0);
    let usage = usage_payload(&app).await;
    assert_eq!(usage["web_search_calls"], 1, "{usage}");
    assert_eq!(usage["file_search_calls"], 1, "{usage}");
    assert_eq!(usage["code_interpreter_calls"], 0, "{usage}");
    let daily = quota_row(&app, u.tenant, u.user, Period::Daily, Bucket::Total)
        .await
        .expect("daily total row");
    assert_eq!(daily.web_search_calls, 1);
}

#[tokio::test]
async fn web_search_limit_exceeded_mid_turn() {
    let app = TestApp::builder().build().await;
    assert_eq!(test_config().quota.web_search_max_calls_per_message, 2);
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(
        &app,
        Responder::Sse(vec![
            provider_event("response.web_search_call.searching"),
            provider_event("response.web_search_call.searching"),
            provider_event("response.web_search_call.searching"),
            text_delta("never sent"),
            completed(10, 4),
        ]),
    );
    let request_id = Uuid::new_v4();

    let frames = send(
        &app,
        &u,
        chat,
        json!({"content": "search", "request_id": request_id,
               "web_search": {"enabled": true}}),
    )
    .await;

    assert_eq!(
        event_names(&frames),
        ["stream_started", "tool", "tool", "error"]
    );
    assert_eq!(
        frame(&frames, "error").data["code"],
        "web_search_calls_exceeded"
    );
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("web_search_calls_exceeded")
    );
    assert_eq!(turn.assistant_message_id, None);
    let usage = usage_payload(&app).await;
    assert_eq!(usage["billing_outcome"], "failed");
    assert_eq!(usage["settlement_method"], "estimated");
    assert_eq!(daily_spent(&app, &u).await, estimated_credits(&turn));
    TestApp::wait_until("the provider stream is dropped", || async {
        app.gateway.dropped_bodies() == 1
    })
    .await;
}

#[tokio::test]
async fn code_interpreter_limit_exceeded_mid_turn() {
    let mut cfg = test_config();
    cfg.quota.code_interpreter_max_calls_per_message = 1;
    let app = TestApp::builder().config(cfg).build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    seed_attachment(
        &app,
        SeedAttachment {
            for_file_search: false,
            for_code_interpreter: true,
            ..SeedAttachment::document(u.tenant, chat, u.user)
        },
    )
    .await;
    script_provider(
        &app,
        Responder::Sse(vec![
            provider_event("response.code_interpreter_call.in_progress"),
            provider_event("response.code_interpreter_call.in_progress"),
            completed(10, 4),
        ]),
    );
    let request_id = Uuid::new_v4();

    let frames = send(
        &app,
        &u,
        chat,
        json!({"content": "compute", "request_id": request_id}),
    )
    .await;

    assert!(
        provider_calls(&app)[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "code_interpreter"),
        "code_interpreter is part of the request"
    );
    assert_eq!(event_names(&frames), ["stream_started", "tool", "error"]);
    assert_eq!(
        frame(&frames, "error").data["code"],
        "code_interpreter_calls_exceeded"
    );
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("code_interpreter_calls_exceeded")
    );
    let usage = usage_payload(&app).await;
    assert_eq!(usage["settlement_method"], "estimated");
}

#[tokio::test]
async fn provider_http_errors_map_to_sse_codes() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider_sequence(
        &app,
        vec![
            Responder::json(
                500,
                json!({"error": {"message": "File file-AbCdEfGhIjKlMn could not be read"}}),
            ),
            Responder::json_with_headers(
                429,
                &[("retry-after", "7")],
                json!({"error": {"message": "Rate limit reached"}}),
            ),
            Responder::GatewayStatus(504),
        ],
    );

    let mut errors = Vec::new();
    for (n, code) in ["provider_error", "rate_limited", "provider_timeout"]
        .into_iter()
        .enumerate()
    {
        let request_id = Uuid::new_v4();
        let frames = send(
            &app,
            &u,
            chat,
            json!({"content": format!("q{n}"), "request_id": request_id}),
        )
        .await;
        assert_eq!(event_names(&frames), ["stream_started", "error"], "{code}");
        let error = frames[1].data.clone();
        assert_eq!(error["code"], code, "{error}");
        let turn = turn_of(&app, chat, request_id).await;
        assert_eq!(turn.state, "failed", "{code}");
        assert_eq!(turn.error_code.as_deref(), Some(code));
        errors.push(error);
    }
    let provider_error = errors[0]["message"].as_str().unwrap();
    assert!(!provider_error.contains("file-"), "{provider_error}");
    let rate_limited = errors[1]["message"].as_str().unwrap();
    assert!(rate_limited.contains("retry in 7s"), "{rate_limited}");

    for usage in app.outbox_payloads(USAGE_QUEUE) {
        assert_eq!(usage["billing_outcome"], "failed", "{usage}");
        assert_eq!(usage["settlement_method"], "estimated", "{usage}");
    }
}

#[tokio::test]
async fn response_failed_with_usage_settles_actual() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(
        &app,
        Responder::Sse(vec![failed_with_usage("model overloaded", 10, 2)]),
    );

    let frames = send(&app, &u, chat, json!({"content": "hi"})).await;

    assert_eq!(event_names(&frames), ["stream_started", "error"]);
    assert_eq!(frames[1].data["code"], "provider_error");
    let usage = usage_payload(&app).await;
    assert_eq!(usage["billing_outcome"], "failed");
    assert_eq!(usage["settlement_method"], "actual");
    assert_eq!(usage["actual_credits_micro"], premium_credits(10, 2));
    assert_eq!(daily_spent(&app, &u).await, premium_credits(10, 2));
}

#[tokio::test]
async fn incomplete_is_completed_without_error_code() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(
        &app,
        Responder::Sse(vec![
            text_delta("Trunc"),
            annotation(
                0,
                &json!({"type": "url_citation", "url": "https://e.example", "title": "E",
                       "start_index": 0, "end_index": 2}),
            ),
            incomplete("max_output_tokens", 8, 3),
        ]),
    );
    let request_id = Uuid::new_v4();

    let frames = send(
        &app,
        &u,
        chat,
        json!({"content": "long answer please", "request_id": request_id}),
    )
    .await;

    assert_eq!(event_names(&frames), ["stream_started", "delta", "done"]);
    let done = &frames[2].data;
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 8, "output_tokens": 3})
    );
    assert!(
        done["quota_warnings"]
            .as_array()
            .is_some_and(|w| !w.is_empty()),
        "{done}"
    );
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.error_code, None);
    let assistant = messages_of(&app, chat).await.pop().unwrap();
    assert_eq!(assistant.content, "Trunc");
    let usage = usage_payload(&app).await;
    assert_eq!(usage["settlement_method"], "actual");
}

#[tokio::test]
async fn disconnect_mid_stream_cancels_turn_and_settles_estimated() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(
        &app,
        Responder::Sse(vec![text_delta("partial"), SseScript::Hang]),
    );
    let request_id = Uuid::new_v4();

    let mut reader = open(
        &app,
        &u,
        chat,
        json!({"content": "hi", "request_id": request_id}),
    )
    .await;
    let started = expect_frame(&mut reader, "stream_started").await;
    expect_frame(&mut reader, "delta").await;
    drop(reader);

    TestApp::wait_until("the turn is cancelled", || async {
        turn_of(&app, chat, request_id).await.state == "cancelled"
    })
    .await;
    let turn = turn_of(&app, chat, request_id).await;
    let message_id = uuid_of(&started.data["message_id"]);
    assert_eq!(turn.assistant_message_id, Some(message_id));
    assert_eq!(turn.error_code, None);
    let assistant = messages_of(&app, chat).await.pop().unwrap();
    assert_eq!(
        (
            assistant.id,
            assistant.role.as_str(),
            assistant.content.as_str()
        ),
        (message_id, "assistant", "partial")
    );
    let usage = usage_payload(&app).await;
    assert_eq!(usage["billing_outcome"], "aborted");
    assert_eq!(usage["settlement_method"], "estimated");
    assert_eq!(daily_spent(&app, &u).await, estimated_credits(&turn));
    wait_for_payloads(&app, AUDIT_QUEUE, 1).await;
    assert_eq!(
        app.outbox_payloads(AUDIT_QUEUE)[0]["event_type"],
        "turn_failed"
    );
    assert_eq!(
        app.gateway.dropped_bodies(),
        1,
        "the provider stream is dropped"
    );
}

#[tokio::test]
async fn disconnect_before_any_text_persists_no_message() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, Responder::Sse(vec![SseScript::Hang]));
    let request_id = Uuid::new_v4();

    let mut reader = open(
        &app,
        &u,
        chat,
        json!({"content": "hi", "request_id": request_id}),
    )
    .await;
    expect_frame(&mut reader, "stream_started").await;
    drop(reader);

    TestApp::wait_until("the turn is cancelled", || async {
        turn_of(&app, chat, request_id).await.state == "cancelled"
    })
    .await;
    assert_eq!(
        turn_of(&app, chat, request_id).await.assistant_message_id,
        None
    );
    let msgs = messages_of(&app, chat).await;
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert_eq!(msgs[0].role, "user");
    assert_eq!(usage_payload(&app).await["billing_outcome"], "aborted");
}

#[tokio::test]
async fn message_persistence_failure_reports_error() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    let gate = Arc::new(Notify::new());
    script_provider(
        &app,
        Responder::Sse(vec![
            text_delta("answer"),
            SseScript::Gate(Arc::clone(&gate)),
            completed(7, 3),
        ]),
    );
    let request_id = Uuid::new_v4();

    let mut reader = open(
        &app,
        &u,
        chat,
        json!({"content": "hi", "request_id": request_id}),
    )
    .await;
    expect_frame(&mut reader, "stream_started").await;
    expect_frame(&mut reader, "delta").await;
    seed_assistant_message(&app, u.tenant, chat, request_id).await;
    gate.notify_one();
    let rest = reader.collect().await;

    assert_eq!(event_names(&rest), ["error"]);
    assert_eq!(rest[0].data["code"], "message_persistence_failed");
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("message_persistence_failed")
    );
    assert_eq!(turn.assistant_message_id, None);
    let usage = usage_payload(&app).await;
    assert_eq!(usage["billing_outcome"], "failed");
    assert_eq!(usage["settlement_method"], "actual");
    assert_eq!(daily_spent(&app, &u).await, premium_credits(7, 3));
}

#[tokio::test]
async fn finalization_failure_reports_finalization_failed() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["huge"], 20_000_000, 1));
    let request_id = Uuid::new_v4();

    let frames = send(
        &app,
        &u,
        chat,
        json!({"content": "hi", "request_id": request_id}),
    )
    .await;

    assert_eq!(event_names(&frames), ["stream_started", "delta", "error"]);
    assert_eq!(frames[2].data["code"], "finalization_failed");
    assert_eq!(turn_of(&app, chat, request_id).await.state, "running");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(app.outbox_payloads(USAGE_QUEUE).is_empty());
}

#[tokio::test]
async fn cas_lost_emits_stream_interrupted() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    let gate = Arc::new(Notify::new());
    script_provider(
        &app,
        Responder::Sse(vec![SseScript::Gate(Arc::clone(&gate)), completed(5, 5)]),
    );
    let request_id = Uuid::new_v4();

    let mut reader = open(
        &app,
        &u,
        chat,
        json!({"content": "hi", "request_id": request_id}),
    )
    .await;
    expect_frame(&mut reader, "stream_started").await;
    let turn = turn_of(&app, chat, request_id).await;
    set_turn_state(&app, turn.id, "failed", Some("orphan_timeout")).await;
    let quota_before = quota_rows(&app, u.tenant, u.user).await;
    assert!(
        quota_before.iter().all(|r| r.reserved_credits_micro > 0),
        "the turn's reserve is booked: {quota_before:?}"
    );
    gate.notify_one();
    let rest = reader.collect().await;

    assert_eq!(event_names(&rest), ["error"]);
    assert_eq!(rest[0].data["code"], "stream_interrupted");
    assert_eq!(
        quota_rows(&app, u.tenant, u.user).await,
        quota_before,
        "the CAS loser settles nothing"
    );
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(
        (turn.state.as_str(), turn.error_code.as_deref()),
        ("failed", Some("orphan_timeout"))
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(app.outbox_payloads(USAGE_QUEUE).is_empty());
}

#[tokio::test]
async fn unexpected_function_call_fails_turn() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(
        &app,
        Responder::Sse(vec![function_call("search_knowledge"), completed(5, 5)]),
    );
    let request_id = Uuid::new_v4();

    let frames = send(
        &app,
        &u,
        chat,
        json!({"content": "hi", "request_id": request_id}),
    )
    .await;

    assert_eq!(event_names(&frames), ["stream_started", "error"]);
    assert_eq!(frames[1].data["code"], "unexpected_tool_use");
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("unexpected_tool_use"));
}

#[tokio::test(start_paused = true)]
async fn relay_pings_only_before_content() {
    let (tx, rx) = mpsc::channel(8);
    let start = tokio::time::Instant::now();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(31)).await;
        tx.send(StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: "a".into(),
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_secs(100)).await;
        // Closed without a terminal event.
    });

    let mut relayed = std::pin::pin!(relay::with_pings(rx, Duration::from_secs(15)));
    let mut seen = Vec::new();
    while let Some(event) = futures::StreamExt::next(&mut relayed).await {
        seen.push((start.elapsed().as_secs(), event));
    }

    let delta = StreamEvent::Delta {
        kind: DeltaKind::Text,
        content: "a".into(),
    };
    let last = seen.pop().expect("a terminal event");
    assert_eq!(
        seen,
        [
            (15, StreamEvent::Ping),
            (30, StreamEvent::Ping),
            (31, delta)
        ]
    );
    assert!(
        matches!(&last, (131, StreamEvent::Error { code, .. }) if code == "stream_interrupted"),
        "{last:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn relay_ends_after_terminal_event() {
    let (tx, rx) = mpsc::channel(8);
    let error = StreamEvent::Error {
        code: "provider_error".into(),
        message: "m".into(),
    };
    tx.send(error.clone()).await.unwrap();
    tx.send(StreamEvent::Ping).await.unwrap();

    let relayed = relay::with_pings(rx, Duration::from_secs(15));
    // The sender stays open: the relay must end on its own after the terminal event.
    let seen = tokio::time::timeout(
        Duration::from_secs(3600),
        futures::StreamExt::collect::<Vec<_>>(relayed),
    )
    .await
    .expect("the relay ends after the terminal event");
    assert_eq!(seen, [error]);
    drop(tx);
}

#[tokio::test]
async fn null_content_completion_persists_empty_message() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, Responder::Sse(vec![completed(3, 0)]));
    let request_id = Uuid::new_v4();
    let body = json!({"content": "hi", "request_id": request_id});

    let frames = send(&app, &u, chat, body.clone()).await;

    assert_eq!(event_names(&frames), ["stream_started", "done"]);
    let turn = turn_of(&app, chat, request_id).await;
    assert_eq!(turn.state, "completed");
    let assistant = messages_of(&app, chat).await.pop().unwrap();
    assert_eq!(assistant.role, "assistant");
    assert_eq!(assistant.content, "");
    assert_eq!(turn.assistant_message_id, Some(assistant.id));

    let replay = send(&app, &u, chat, body).await;
    assert_eq!(event_names(&replay), ["stream_started", "delta", "done"]);
    assert_eq!(replay[1].data, json!({"type": "text", "content": ""}));
}

// ---- Task 24: acceptance-criteria gaps --------------------------------------------------------

/// The context budget is computed for the effective model after a downgrade: a request that fits
/// the selected premium model but not the downgraded one is rejected with 400
/// `CONTEXT_BUDGET_EXCEEDED` before any provider call, leaving no turn, message or reserve.
#[tokio::test]
async fn assembled_request_over_the_downgraded_model_budget_is_rejected() {
    let mut premium = crate::test_support::catalog::premium_model("gpt-premium");
    premium.preference = Some(mini_chat_sdk::ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    let mut small = crate::test_support::catalog::standard_model("std-small");
    // Input budget 300 - 100 fixed overhead; the system prompt alone is ~1100 tokens.
    small.max_input_tokens = 300;
    small.system_prompt = "Be brief. ".repeat(400);
    let app = TestApp::builder()
        .catalog(vec![premium, small])
        .build()
        .await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, Some("gpt-premium")).await;
    script_provider(&app, answer(&["ok"], 3, 2));
    send(&app, &u, chat, json!({"content": "hi"})).await;
    let calls_before = provider_calls(&app).len();
    let turns_before = turns_of(&app, chat).await.len();
    let messages_before = messages_of(&app, chat).await.len();
    seed_spent(
        &app,
        u.tenant,
        u.user,
        Period::Daily,
        Bucket::Premium,
        PREMIUM_LIMITS.limit_daily_credits_micro,
    )
    .await;

    let res = app
        .call(
            "POST",
            &stream_uri(chat),
            &u.who,
            Some(json!({"content": "hi"})),
        )
        .await;

    assert_eq!(res.status, 400, "{}", res.json);
    let v = &res.json["context"]["field_violations"][0];
    assert_eq!(v["reason"], "CONTEXT_BUDGET_EXCEEDED", "{}", res.json);
    assert_eq!(provider_calls(&app).len(), calls_before);
    assert_eq!(turns_of(&app, chat).await.len(), turns_before);
    assert_eq!(messages_of(&app, chat).await.len(), messages_before);
    for row in quota_rows(&app, u.tenant, u.user).await {
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
    }
}

/// A cancelled turn frees the chat for a new turn, while its own `request_id` stays a
/// `request_id_conflict`.
#[tokio::test]
async fn cancelled_turn_frees_the_chat_but_not_its_request_id() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider_sequence(
        &app,
        vec![
            Responder::Sse(vec![SseScript::Hang]),
            answer(&["next"], 2, 1),
        ],
    );
    let cancelled_id = Uuid::new_v4();
    let mut running = open(
        &app,
        &u,
        chat,
        json!({"content": "a", "request_id": cancelled_id}),
    )
    .await;
    expect_frame(&mut running, "stream_started").await;
    let blocked = app
        .call(
            "POST",
            &stream_uri(chat),
            &u.who,
            Some(json!({"content": "b"})),
        )
        .await;
    assert_eq!(blocked.status, 409, "{}", blocked.json);
    assert_eq!(blocked.json["context"]["reason"], "turn_already_running");

    drop(running);
    TestApp::wait_until("the turn is cancelled", || async {
        turn_of(&app, chat, cancelled_id).await.state == "cancelled"
    })
    .await;

    let frames = send(&app, &u, chat, json!({"content": "b"})).await;
    assert_eq!(event_names(&frames), ["stream_started", "delta", "done"]);
    let reused = app
        .call(
            "POST",
            &stream_uri(chat),
            &u.who,
            Some(json!({"content": "a", "request_id": cancelled_id})),
        )
        .await;
    assert_eq!(reused.status, 409, "{}", reused.json);
    assert_eq!(reused.json["context"]["reason"], "request_id_conflict");
    assert_eq!(provider_calls(&app).len(), 2);
}

/// The reserve is booked on every bucket of the turn before the provider call and stays booked
/// while the provider streams; the settlement releases it.
#[tokio::test]
async fn reserve_is_booked_while_the_provider_call_runs() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, Responder::Sse(vec![SseScript::Hang]));
    let request_id = Uuid::new_v4();

    let mut reader = open(
        &app,
        &u,
        chat,
        json!({"content": "hi", "request_id": request_id}),
    )
    .await;
    expect_frame(&mut reader, "stream_started").await;
    TestApp::wait_until("the provider call is in flight", || async {
        provider_calls(&app).len() == 1
    })
    .await;

    let turn = turn_of(&app, chat, request_id).await;
    let reserved = turn
        .reserved_credits_micro
        .expect("reserve persisted on the turn");
    assert!(reserved > 0);
    assert!(turn.reserve_tokens.is_some_and(|t| t > 0));
    let rows = quota_rows(&app, u.tenant, u.user).await;
    assert_eq!(
        rows.len(),
        4,
        "daily and monthly, total and premium: {rows:?}"
    );
    for row in &rows {
        assert_eq!(row.reserved_credits_micro, reserved, "{row:?}");
        assert_eq!(row.spent_credits_micro, 0, "{row:?}");
    }

    drop(reader);
    TestApp::wait_until("the turn is cancelled", || async {
        turn_of(&app, chat, request_id).await.state == "cancelled"
    })
    .await;
    for row in quota_rows(&app, u.tenant, u.user).await {
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
        assert!(row.spent_credits_micro > 0, "{row:?}");
    }
}

/// A downgraded turn is charged at the effective (standard) model's multipliers, on the `total`
/// bucket only; the chat keeps its selected model.
#[tokio::test]
async fn downgraded_turn_is_charged_at_the_effective_model_rate() {
    const STANDARD_MULT: i64 = 1_000_000;
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, Some("gpt-premium")).await;
    seed_spent(
        &app,
        u.tenant,
        u.user,
        Period::Daily,
        Bucket::Premium,
        PREMIUM_LIMITS.limit_daily_credits_micro,
    )
    .await;
    script_provider(&app, answer(&["ok"], 40, 20));

    let frames = send(&app, &u, chat, json!({"content": "hi"})).await;

    assert_eq!(
        frame(&frames, "done").data["effective_model"],
        "gpt-standard"
    );
    let expected = credits_micro_checked(40, 20, STANDARD_MULT, STANDARD_MULT).unwrap();
    assert_ne!(expected, premium_credits(40, 20));
    for period in [Period::Daily, Period::Monthly] {
        let total = quota_row(&app, u.tenant, u.user, period, Bucket::Total)
            .await
            .expect("total row");
        assert_eq!(
            (
                total.spent_credits_micro,
                total.reserved_credits_micro,
                total.calls
            ),
            (expected, 0, 1),
            "{period:?}"
        );
        assert_eq!((total.input_tokens, total.output_tokens), (40, 20));
    }
    let premium_daily = quota_row(&app, u.tenant, u.user, Period::Daily, Bucket::Premium)
        .await
        .expect("seeded premium row");
    assert_eq!(
        (
            premium_daily.spent_credits_micro,
            premium_daily.reserved_credits_micro,
            premium_daily.calls
        ),
        (PREMIUM_LIMITS.limit_daily_credits_micro, 0, 0)
    );
    assert!(
        quota_row(&app, u.tenant, u.user, Period::Monthly, Bucket::Premium)
            .await
            .is_none()
    );
    let usage = usage_payload(&app).await;
    assert_eq!(usage["effective_model"], "gpt-standard");
    assert_eq!(usage["selected_model"], "gpt-premium");
    assert_eq!(usage["actual_credits_micro"], expected);
    assert_eq!(chat_row(&app, chat).await.model, "gpt-premium");
}

/// `GET /quota/status` reports exactly what the settled turns charged.
#[tokio::test]
async fn quota_status_matches_the_settled_turns() {
    // The turns and the status read must fall into one UTC day (and month).
    let now = time::OffsetDateTime::now_utc();
    let to_midnight = crate::domain::quota::periods::next_reset(Period::Daily, now) - now;
    if to_midnight < time::Duration::seconds(30) {
        tokio::time::sleep((to_midnight + time::Duration::seconds(1)).unsigned_abs()).await;
    }
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["ok"], 12, 5));
    send(&app, &u, chat, json!({"content": "one"})).await;
    send(&app, &u, chat, json!({"content": "two"})).await;
    let charged = 2 * premium_credits(12, 5);
    assert_eq!(daily_spent(&app, &u).await, charged);

    let res = app
        .call("GET", "/mini-chat/v1/quota/status", &u.who, None)
        .await;

    assert_eq!(res.status, 200, "{}", res.json);
    let tiers = res.json["tiers"].as_array().expect("tiers");
    let names: Vec<&str> = tiers.iter().map(|t| t["tier"].as_str().unwrap()).collect();
    assert_eq!(names, ["premium", "total"], "{}", res.json);
    for tier in tiers {
        let limits = if tier["tier"] == "premium" {
            PREMIUM_LIMITS
        } else {
            crate::test_support::app::STANDARD_LIMITS
        };
        let periods = tier["periods"].as_array().expect("periods");
        let period_names: Vec<&str> = periods
            .iter()
            .map(|p| p["period"].as_str().unwrap())
            .collect();
        assert_eq!(period_names, ["daily", "monthly"], "{tier}");
        for period in periods {
            let limit = if period["period"] == "daily" {
                limits.limit_daily_credits_micro
            } else {
                limits.limit_monthly_credits_micro
            };
            assert_eq!(period["limit_credits_micro"], limit, "{period}");
            assert_eq!(period["used_credits_micro"], charged, "{period}");
            assert_eq!(
                period["remaining_credits_micro"],
                limit - charged,
                "{period}"
            );
        }
    }
}

/// The usage event of a turn reaches the policy plugin even when the first delivery fails
/// transiently; every delivery carries the turn's dedupe key, and a replay publishes nothing.
#[tokio::test]
async fn usage_is_published_once_per_turn_and_redelivered_after_a_transient_failure() {
    let app = TestApp::builder().build().await;
    let u = new_user();
    let chat = create_chat(&app, &u.who, None).await;
    script_provider(&app, answer(&["ok"], 12, 5));
    app.usage
        .fail_next_publish(mini_chat_sdk::PublishError::Transient(
            "plugin down".to_owned(),
        ));
    let request_id = Uuid::new_v4();
    let body = json!({"content": "hi", "request_id": request_id});

    send(&app, &u, chat, body.clone()).await;

    TestApp::wait_until("the usage event is redelivered", || async {
        app.usage.usage_events().len() >= 2
    })
    .await;
    let turn = turn_of(&app, chat, request_id).await;
    let key = format!(
        "{}/{}/{}",
        u.tenant.simple(),
        turn.id.simple(),
        request_id.simple()
    );
    send(&app, &u, chat, body).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let published = app.usage.usage_events();
    assert_eq!(published.len(), 2, "one failed and one successful delivery");
    assert_eq!(published[0], published[1], "the same event is redelivered");
    assert_eq!(published[0].dedupe_key, key);
    assert_eq!(published[0].turn_id, Some(turn.id));
    assert_eq!(published[0].billing_outcome, "completed");
}
