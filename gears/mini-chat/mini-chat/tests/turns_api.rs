//! Turn mutations: `POST /v1/chats/{id}/turns/{request_id}/retry`,
//! `PATCH /v1/chats/{id}/turns/{request_id}` and `DELETE …/turns/{request_id}`
//! (DESIGN §3.9, §3.6 "Retry / edit variant", §5.7 "Unstarted retry/edit turn").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Utc};
use mini_chat::domain::model::TurnState;
use mini_chat::infra::db::entities::{
    attachment, chat, chat_turn, message, message_attachment, quota_usage, thread_summary,
};
use mini_chat::infra::db::repos::TurnRepo;
use mini_chat::infra::db::repos::turn::{NewTurn, TerminalUpdate, TurnCounters};
use mini_chat::infra::outbox::QueueKind;
use mini_chat::testing::catalog;
use mini_chat::testing::seed::{self, NewAttachment, NewMessage};
use mini_chat::testing::{ScriptedStream, SseCapture, TestApp, TestUser};
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

fn uuid_of(v: &Value) -> Uuid {
    Uuid::parse_str(v.as_str().expect("uuid string")).expect("uuid")
}

/// Send `content` with a fresh request id and wait for the terminal event.
async fn send(app: &TestApp, chat: Uuid, content: &str) -> Uuid {
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
    request_id
}

async fn retry(app: &TestApp, user: TestUser, chat: Uuid, request_id: Uuid) -> SseCapture {
    app.stream_with(
        user,
        Method::POST,
        &format!("{}/retry", turn_path(chat, request_id)),
        json!({}),
    )
    .await
}

async fn edit(
    app: &TestApp,
    user: TestUser,
    chat: Uuid,
    request_id: Uuid,
    content: &str,
) -> SseCapture {
    app.stream_with(
        user,
        Method::PATCH,
        &turn_path(chat, request_id),
        json!({"content": content}),
    )
    .await
}

async fn delete(
    app: &TestApp,
    user: TestUser,
    chat: Uuid,
    request_id: Uuid,
) -> (StatusCode, Value) {
    let r = app
        .call(user, Method::DELETE, &turn_path(chat, request_id), None)
        .await;
    (r.status, r.json)
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

async fn links_of(app: &TestApp, message_id: Uuid) -> Vec<Uuid> {
    let conn = app.db.conn().unwrap();
    message_attachment::Entity::find()
        .filter(message_attachment::Column::MessageId.eq(message_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.attachment_id)
        .collect()
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

/// Audit events of `event_type` delivered after a short settling wait.
async fn audits_now(app: &TestApp, event_type: &str) -> Vec<Value> {
    tokio::time::sleep(Duration::from_millis(300)).await;
    app.outbox_messages_n(QueueKind::Audit, 0)
        .await
        .into_iter()
        .filter(|e| e["event_type"] == event_type)
        .collect()
}

/// Audit events of `event_type` delivered so far (waits for at least `n` of them).
async fn audits(app: &TestApp, event_type: &str, n: usize) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let found: Vec<Value> = app
            .outbox_messages(QueueKind::Audit)
            .await
            .into_iter()
            .filter(|e| e["event_type"] == event_type)
            .collect();
        if found.len() >= n || tokio::time::Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_problem(c: &SseCapture, status: StatusCode) -> &Value {
    assert_eq!(c.status, status, "{c:?}");
    c.problem.as_ref().expect("problem body")
}

fn assert_not_latest(c: &SseCapture) {
    let p = assert_problem(c, StatusCode::CONFLICT);
    assert_eq!(p["context"]["reason"], "NOT_LATEST_TURN", "{p}");
}

/// Seed a terminal turn of `requester` in `chat` with its user (and, for a
/// completed turn, assistant) message; `started_at` = `at`.
async fn seed_turn(
    app: &TestApp,
    chat: Uuid,
    requester: Uuid,
    state: TurnState,
    content: &str,
    at: DateTime<Utc>,
) -> Uuid {
    let request_id = Uuid::new_v4();
    let conn = app.db.conn().unwrap();
    let turn = TurnRepo::insert_running(
        &conn,
        NewTurn {
            id: Uuid::new_v4(),
            tenant_id: U.tenant_id,
            chat_id: chat,
            request_id,
            requester_user_id: requester,
            web_search_enabled: false,
            preflight: None,
            now: at,
        },
    )
    .await
    .unwrap();
    seed::insert_message(&app.db, chat, "user", content, Some(request_id), at).await;
    if state == TurnState::Running {
        return request_id;
    }
    let assistant = if state == TurnState::Completed {
        Some(
            seed::insert_message_with(
                &app.db,
                NewMessage::new(
                    chat,
                    "assistant",
                    "seeded answer",
                    Some(request_id),
                    at + chrono::Duration::milliseconds(1),
                )
                .model("gpt-premium")
                .tokens(10, 5),
            )
            .await,
        )
    } else {
        None
    };
    assert!(
        TurnRepo::cas_finalize(
            &conn,
            turn.id,
            &TerminalUpdate {
                state,
                error_code: None,
                error_detail: None,
                assistant_message_id: assistant,
                provider_response_id: None,
                counters: TurnCounters::default(),
                now: at,
            },
        )
        .await
        .unwrap()
    );
    request_id
}

// ---------------------------------------------------------------------------------------------
// retry / edit / delete
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn retry_latest_creates_new_turn_with_server_request_id() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let old = send(&app, chat, "hi").await;
    let before = chat_row(&app, chat).await.updated_at;
    app.provider
        .push_stream(ScriptedStream::text(&["again"], 10, 5));

    let c = retry(&app, U, chat, old).await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    assert_eq!(c.names(), ["stream_started", "delta", "done"], "{c:?}");
    let started = c.first("stream_started").unwrap();
    assert_eq!(started["is_new_turn"], true);
    let new = uuid_of(&started["request_id"]);
    assert_ne!(new, old);
    assert_eq!(new.get_version_num(), 4);

    let old_turn = turn_of(&app, chat, old).await;
    assert!(old_turn.deleted_at.is_some());
    assert_eq!(old_turn.replaced_by_request_id, Some(new));
    let new_turn = turn_of(&app, chat, new).await;
    assert_eq!(new_turn.state, "completed");
    assert!(new_turn.deleted_at.is_none());
    assert_eq!(new_turn.requester_user_id, Some(U.user_id));
    assert_eq!(new_turn.effective_model.as_deref(), Some("gpt-premium"));
    assert!(new_turn.reserve_tokens.is_some());

    let msgs = messages(&app, chat).await;
    assert_eq!(msgs.len(), 4);
    for m in msgs.iter().filter(|m| m.request_id == Some(old)) {
        assert!(m.deleted_at.is_some(), "old message not deleted: {m:?}");
    }
    let new_msgs: Vec<_> = msgs.iter().filter(|m| m.request_id == Some(new)).collect();
    assert_eq!(new_msgs.len(), 2);
    assert_eq!(new_msgs[0].role, "user");
    assert_eq!(new_msgs[0].content, "hi");
    assert_eq!(new_msgs[1].role, "assistant");
    assert_eq!(new_msgs[1].content, "again");

    let r = app
        .call(U, Method::GET, &format!("{CHATS}/{chat}/messages"), None)
        .await;
    let items = r.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{}", r.json);
    assert!(items.iter().all(|m| uuid_of(&m["request_id"]) == new));

    // The provider got the original content without the replaced answer.
    let req = app.provider.chat_requests().last().unwrap().clone();
    let input = req["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "{req}");
    assert_eq!(input[0]["content"][0]["text"], "hi");

    let ev = audits(&app, "turn_retry", 1).await;
    assert_eq!(ev.len(), 1);
    assert_eq!(uuid_of(&ev[0]["original_request_id"]), old);
    assert_eq!(uuid_of(&ev[0]["new_request_id"]), new);
    assert_eq!(uuid_of(&ev[0]["actor_user_id"]), U.user_id);
    assert_eq!(uuid_of(&ev[0]["chat_id"]), chat);
    assert!(ev[0]["timestamp"].is_string());

    assert!(chat_row(&app, chat).await.updated_at > before);
}

#[tokio::test]
async fn edit_replaces_content() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let first = send(&app, chat, "first question").await;
    let old = send(&app, chat, "typo").await;

    let c = edit(&app, U, chat, old, "fixed").await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    assert_eq!(c.last().unwrap().0, "done", "{c:?}");
    let new = uuid_of(&c.first("stream_started").unwrap()["request_id"]);
    assert_ne!(new, old);

    let old_turn = turn_of(&app, chat, old).await;
    assert!(old_turn.deleted_at.is_some());
    assert_eq!(old_turn.replaced_by_request_id, Some(new));
    assert!(turn_of(&app, chat, first).await.deleted_at.is_none());

    let live: Vec<_> = messages(&app, chat)
        .await
        .into_iter()
        .filter(|m| m.deleted_at.is_none())
        .collect();
    assert_eq!(live.len(), 4);
    assert_eq!(live[2].role, "user");
    assert_eq!(live[2].content, "fixed");
    assert_eq!(live[2].request_id, Some(new));

    // History: the first turn, then the edited message; the replaced turn is gone.
    let req = app.provider.chat_requests().last().unwrap().clone();
    let texts: Vec<&str> = req["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            m["content"]
                .as_str()
                .or_else(|| m["content"][0]["text"].as_str())
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(texts.first().copied(), Some("first question"), "{req}");
    assert_eq!(texts.last().copied(), Some("fixed"), "{req}");
    assert!(!texts.contains(&"typo"), "{req}");

    let ev = audits(&app, "turn_edit", 1).await;
    assert_eq!(ev.len(), 1);
    assert_eq!(uuid_of(&ev[0]["original_request_id"]), old);
    assert_eq!(uuid_of(&ev[0]["new_request_id"]), new);
}

#[tokio::test]
async fn edit_empty_content_400() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let old = send(&app, chat, "hi").await;
    let c = edit(&app, U, chat, old, "   ").await;
    let p = assert_problem(&c, StatusCode::BAD_REQUEST);
    let v = &p["context"]["field_violations"][0];
    assert_eq!(v["field"], "content", "{p}");
    assert_eq!(v["reason"], "EMPTY_CONTENT", "{p}");
    assert!(turn_of(&app, chat, old).await.deleted_at.is_none());
    assert_eq!(turns(&app, chat).await.len(), 1);
}

#[tokio::test]
async fn delete_latest_204_and_audit_turn_delete() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let first = send(&app, chat, "one").await;
    let old = send(&app, chat, "two").await;
    let before = chat_row(&app, chat).await.updated_at;

    let (status, body) = delete(&app, U, chat, old).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(body, Value::Null);

    let turn = turn_of(&app, chat, old).await;
    assert!(turn.deleted_at.is_some());
    assert_eq!(turn.replaced_by_request_id, None);
    assert_eq!(turn.state, "completed");
    assert!(turn_of(&app, chat, first).await.deleted_at.is_none());
    for m in messages(&app, chat).await {
        assert_eq!(m.deleted_at.is_some(), m.request_id == Some(old), "{m:?}");
    }
    assert_eq!(turns(&app, chat).await.len(), 2, "no new turn");
    assert_eq!(chat_row(&app, chat).await.updated_at, before);

    let ev = audits(&app, "turn_delete", 1).await;
    assert_eq!(ev.len(), 1);
    assert_eq!(uuid_of(&ev[0]["request_id"]), old);
    assert_eq!(uuid_of(&ev[0]["actor_user_id"]), U.user_id);
    assert_eq!(uuid_of(&ev[0]["chat_id"]), chat);

    let r = app.call(U, Method::GET, &turn_path(chat, old), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);

    let c = app
        .stream(
            U,
            &stream_path(chat),
            json!({"content": "two", "request_id": old}),
        )
        .await;
    let p = assert_problem(&c, StatusCode::CONFLICT);
    assert_eq!(p["context"]["reason"], "request_id_conflict", "{p}");

    // The previous turn is the latest again and can be mutated.
    let (status, body) = delete(&app, U, chat, first).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[tokio::test]
async fn mutation_of_non_latest_409_not_latest_turn() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let older = send(&app, chat, "one").await;
    send(&app, chat, "two").await;

    assert_not_latest(&retry(&app, U, chat, older).await);
    assert_not_latest(&edit(&app, U, chat, older, "x").await);
    let (status, body) = delete(&app, U, chat, older).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["context"]["reason"], "NOT_LATEST_TURN", "{body}");
    assert!(
        turns(&app, chat)
            .await
            .iter()
            .all(|t| t.deleted_at.is_none())
    );
    assert_eq!(app.provider.chat_requests().len(), 2);
}

#[tokio::test]
async fn mutation_of_deleted_turn_409_not_latest_turn() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    send(&app, chat, "one").await;
    let deleted = send(&app, chat, "two").await;
    assert_eq!(
        delete(&app, U, chat, deleted).await.0,
        StatusCode::NO_CONTENT
    );

    assert_not_latest(&retry(&app, U, chat, deleted).await);
    assert_not_latest(&edit(&app, U, chat, deleted, "x").await);
    let (status, body) = delete(&app, U, chat, deleted).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["context"]["reason"], "NOT_LATEST_TURN", "{body}");
}

#[tokio::test]
async fn mutation_of_running_turn_400_turn_state() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.push_stream(ScriptedStream {
        hold_after: Some(1),
        ..ScriptedStream::text(&["x"], 10, 5)
    });
    let (events, conn) = app
        .stream_until(U, &stream_path(chat), json!({"content": "x"}), 1)
        .await;
    let running = uuid_of(&events[0].1["request_id"]);

    let state_violation = |p: &Value| {
        let v = &p["context"]["violations"][0];
        assert_eq!(v["subject"], "turn_state", "{p}");
        assert_eq!(v["type"], "STATE", "{p}");
    };
    state_violation(assert_problem(
        &retry(&app, U, chat, running).await,
        StatusCode::BAD_REQUEST,
    ));
    state_violation(assert_problem(
        &edit(&app, U, chat, running, "y").await,
        StatusCode::BAD_REQUEST,
    ));
    let (status, body) = delete(&app, U, chat, running).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    state_violation(&body);

    app.provider.release();
    assert_eq!(conn.rest().await.last().unwrap().0, "done");
    let turn = turn_of(&app, chat, running).await;
    assert!(turn.deleted_at.is_none());
    assert_eq!(turns(&app, chat).await.len(), 1);
}

#[tokio::test]
async fn mutation_unknown_turn_404() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    send(&app, chat, "one").await;
    let unknown = Uuid::new_v4();
    assert_problem(&retry(&app, U, chat, unknown).await, StatusCode::NOT_FOUND);
    assert_problem(
        &edit(&app, U, chat, unknown, "x").await,
        StatusCode::NOT_FOUND,
    );
    assert_eq!(
        delete(&app, U, chat, unknown).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn foreign_chat_404() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let rid = send(&app, chat, "one").await;
    for other in [TestUser::A2, TestUser::B1] {
        assert_problem(&retry(&app, other, chat, rid).await, StatusCode::NOT_FOUND);
        assert_problem(
            &edit(&app, other, chat, rid, "x").await,
            StatusCode::NOT_FOUND,
        );
        assert_eq!(
            delete(&app, other, chat, rid).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert!(turn_of(&app, chat, rid).await.deleted_at.is_none());
}

#[tokio::test]
async fn mutation_by_other_requester_403() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let rid = seed_turn(
        &app,
        chat,
        TestUser::A2.user_id,
        TurnState::Completed,
        "theirs",
        Utc::now(),
    )
    .await;
    let denied = |p: &Value| assert_eq!(p["context"]["reason"], "AUTHZ_DENIED", "{p}");
    denied(assert_problem(
        &retry(&app, U, chat, rid).await,
        StatusCode::FORBIDDEN,
    ));
    denied(assert_problem(
        &edit(&app, U, chat, rid, "x").await,
        StatusCode::FORBIDDEN,
    ));
    let (status, body) = delete(&app, U, chat, rid).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    denied(&body);
    assert!(turn_of(&app, chat, rid).await.deleted_at.is_none());
}

#[tokio::test]
async fn retry_preflight_rejection_keeps_previous_answer() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let old = send(&app, chat, "hi").await;
    let before = chat_row(&app, chat).await.updated_at;
    // Exhaust every bucket the send created (both tiers, both periods).
    let conn = app.db.conn().unwrap();
    let res = quota_usage::Entity::update_many()
        .col_expr(
            quota_usage::Column::SpentCreditsMicro,
            Expr::value(1_000_000_000_000_i64),
        )
        .filter(quota_usage::Column::UserId.eq(U.user_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    assert_eq!(res.rows_affected, 4);

    let c = retry(&app, U, chat, old).await;
    let p = assert_problem(&c, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(p["context"]["violations"][0]["subject"], "tokens", "{p}");

    let turn = turn_of(&app, chat, old).await;
    assert!(turn.deleted_at.is_none());
    assert_eq!(turn.replaced_by_request_id, None);
    assert_eq!(turns(&app, chat).await.len(), 1);
    assert!(
        messages(&app, chat)
            .await
            .iter()
            .all(|m| m.deleted_at.is_none())
    );
    assert_eq!(messages(&app, chat).await.len(), 2);
    assert_eq!(app.provider.chat_requests().len(), 1);
    assert_eq!(chat_row(&app, chat).await.updated_at, before);
    assert!(audits_now(&app, "turn_retry").await.is_empty());
}

#[tokio::test]
async fn retry_copies_non_deleted_attachments_and_resends_images() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let kept = seed::insert_attachment(
        &app.db,
        NewAttachment::image(chat, U.user_id, "kept.png", None),
    )
    .await;
    let gone = seed::insert_attachment(
        &app.db,
        NewAttachment::image(chat, U.user_id, "gone.png", None),
    )
    .await;
    let conn = app.db.conn().unwrap();
    attachment::Entity::update_many()
        .col_expr(attachment::Column::ProviderFileId, Expr::value("file-gone"))
        .filter(attachment::Column::Id.eq(gone))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    let old = Uuid::new_v4();
    let c = app
        .stream(
            U,
            &stream_path(chat),
            json!({"content": "look", "request_id": old, "attachment_ids": [kept, gone]}),
        )
        .await;
    assert_eq!(c.last().unwrap().0, "done", "{c:?}");
    attachment::Entity::update_many()
        .col_expr(attachment::Column::DeletedAt, Expr::value(Utc::now()))
        .filter(attachment::Column::Id.eq(gone))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    let c = retry(&app, U, chat, old).await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    assert_eq!(c.last().unwrap().0, "done", "{c:?}");
    let new = uuid_of(&c.first("stream_started").unwrap()["request_id"]);

    let req = app.provider.chat_requests().last().unwrap().clone();
    let last = req["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["content"],
        json!([
            {"type": "input_text", "text": "look"},
            {"type": "input_image", "file_id": "file-seed"},
        ])
    );

    let msgs = messages(&app, chat).await;
    let old_user = msgs
        .iter()
        .find(|m| m.request_id == Some(old) && m.role == "user")
        .unwrap();
    let new_user = msgs
        .iter()
        .find(|m| m.request_id == Some(new) && m.role == "user")
        .unwrap();
    assert_eq!(links_of(&app, new_user.id).await, vec![kept]);
    let mut old_links = links_of(&app, old_user.id).await;
    old_links.sort();
    let mut expected = vec![kept, gone];
    expected.sort();
    assert_eq!(old_links, expected, "old links stay for audit");
}

#[tokio::test]
async fn retry_image_guard_rejection_keeps_previous_answer() {
    let mut models = catalog::default_catalog();
    models[0].multimodal_capabilities.clear();
    let app = TestApp::builder().catalog(models).build().await;
    let chat = create_chat(&app, U).await;
    let old = seed_turn(
        &app,
        chat,
        U.user_id,
        TurnState::Completed,
        "look",
        Utc::now(),
    )
    .await;
    let img = seed::insert_attachment(
        &app.db,
        NewAttachment::image(chat, U.user_id, "i.png", None),
    )
    .await;
    let user_msg = messages(&app, chat)
        .await
        .into_iter()
        .find(|m| m.role == "user")
        .unwrap();
    seed::link_attachment(&app.db, chat, user_msg.id, img).await;

    let c = retry(&app, U, chat, old).await;
    let p = assert_problem(&c, StatusCode::BAD_REQUEST);
    assert_eq!(
        p["context"]["field_violations"][0]["reason"], "VISION_NOT_SUPPORTED",
        "{p}"
    );
    assert!(turn_of(&app, chat, old).await.deleted_at.is_none());
    assert_eq!(turns(&app, chat).await.len(), 1);
}

#[tokio::test]
async fn retry_setup_failure_marks_new_turn_failed() {
    let mut models = catalog::default_catalog();
    models[0].context_window = 200;
    let app = TestApp::builder().catalog(models).build().await;
    let chat = create_chat(&app, U).await;
    let old = seed_turn(
        &app,
        chat,
        U.user_id,
        TurnState::Completed,
        "hello",
        Utc::now(),
    )
    .await;

    let c = retry(&app, U, chat, old).await;
    let p = assert_problem(&c, StatusCode::BAD_REQUEST);
    assert_eq!(
        p["context"]["field_violations"][0]["reason"], "CONTEXT_BUDGET_EXCEEDED",
        "{p}"
    );

    // The mutation committed: the old turn is replaced by a failed new turn.
    let old_turn = turn_of(&app, chat, old).await;
    assert!(old_turn.deleted_at.is_some());
    let new = old_turn.replaced_by_request_id.expect("replaced");
    let new_turn = turn_of(&app, chat, new).await;
    assert_eq!(new_turn.state, "failed");
    assert_eq!(
        new_turn.error_code.as_deref(),
        Some("context_length_exceeded")
    );
    assert!(new_turn.completed_at.is_some());
    assert!(new_turn.reserve_tokens.is_none() && new_turn.effective_model.is_none());
    assert!(app.provider.chat_requests().is_empty());
    let conn = app.db.conn().unwrap();
    let reserved: i64 = quota_usage::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
        .iter()
        .map(|r| r.reserved_credits_micro)
        .sum();
    assert_eq!(reserved, 0);
    app.assert_no_outbox(QueueKind::Usage, Duration::from_millis(300))
        .await;
    assert_eq!(audits(&app, "turn_retry", 1).await.len(), 1);
    assert!(audits_now(&app, "turn_failed").await.is_empty());

    // The chat is not blocked by a running turn.
    let (status, body) = delete(&app, U, chat, new).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[tokio::test]
async fn retry_insert_race_409_generation_in_progress() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let t0 = Utc::now() - chrono::Duration::seconds(10);
    let latest = seed_turn(
        &app,
        chat,
        U.user_id,
        TurnState::Completed,
        "latest",
        t0 + chrono::Duration::seconds(1),
    )
    .await;
    // A running turn older than the latest (terminal) turn holds the running index.
    seed_turn(&app, chat, U.user_id, TurnState::Running, "older", t0).await;

    let c = retry(&app, U, chat, latest).await;
    let p = assert_problem(&c, StatusCode::CONFLICT);
    assert_eq!(p["context"]["reason"], "GENERATION_IN_PROGRESS", "{p}");
    assert!(turn_of(&app, chat, latest).await.deleted_at.is_none());
    assert_eq!(turns(&app, chat).await.len(), 2);
    assert!(
        messages(&app, chat)
            .await
            .iter()
            .all(|m| m.deleted_at.is_none())
    );
    assert!(audits_now(&app, "turn_retry").await.is_empty());
}

#[tokio::test]
async fn concurrent_retries_one_wins_other_generation_in_progress() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let old = send(&app, chat, "hi").await;
    let (a, b) = tokio::join!(retry(&app, U, chat, old), retry(&app, U, chat, old));
    let (ok, lost) = if a.status == StatusCode::OK {
        (a, b)
    } else {
        (b, a)
    };
    assert_eq!(ok.status, StatusCode::OK, "{ok:?}");
    assert_eq!(ok.last().unwrap().0, "done", "{ok:?}");
    let p = assert_problem(&lost, StatusCode::CONFLICT);
    let reason = p["context"]["reason"].as_str().unwrap();
    assert!(
        reason == "NOT_LATEST_TURN" || reason == "GENERATION_IN_PROGRESS",
        "{p}"
    );
    let all = turns(&app, chat).await;
    assert_eq!(all.len(), 2);
    assert_eq!(all.iter().filter(|t| t.deleted_at.is_none()).count(), 1);
    assert_eq!(audits(&app, "turn_retry", 1).await.len(), 1);
}

async fn seed_summary(app: &TestApp, chat: Uuid, frontier: &message::Model) {
    let now = Utc::now();
    let row = thread_summary::Model {
        id: Uuid::new_v4(),
        tenant_id: U.tenant_id,
        chat_id: chat,
        summary_text: Some("summary".to_owned()),
        summarized_up_to_created_at: frontier.created_at,
        summarized_up_to_message_id: frontier.id,
        token_estimate: Some(3),
        created_at: now,
        updated_at: now,
    };
    let conn = app.db.conn().unwrap();
    secure_insert::<thread_summary::Entity>(
        row.into_active_model(),
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

#[tokio::test]
async fn mutation_invalidates_covering_summary() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    send(&app, chat, "one").await;
    let latest = send(&app, chat, "two").await;
    let latest_user = messages(&app, chat)
        .await
        .into_iter()
        .find(|m| m.request_id == Some(latest) && m.role == "user")
        .unwrap();
    // Frontier exactly at the mutated turn's user message.
    seed_summary(&app, chat, &latest_user).await;

    let (status, body) = delete(&app, U, chat, latest).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(summary_row(&app, chat).await.is_none());
    assert!(messages(&app, chat).await.iter().all(|m| !m.is_compressed));
}

#[tokio::test]
async fn mutation_keeps_summary_before_the_turn() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let first = send(&app, chat, "one").await;
    let latest = send(&app, chat, "two").await;
    let first_answer = messages(&app, chat)
        .await
        .into_iter()
        .find(|m| m.request_id == Some(first) && m.role == "assistant")
        .unwrap();
    seed_summary(&app, chat, &first_answer).await;

    let c = retry(&app, U, chat, latest).await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    assert!(summary_row(&app, chat).await.is_some());
    let compressed: Vec<_> = messages(&app, chat)
        .await
        .into_iter()
        .filter(|m| m.is_compressed)
        .collect();
    assert_eq!(compressed.len(), 2);
    assert!(compressed.iter().all(|m| m.request_id == Some(first)));
    let started = c.first("stream_started").unwrap();
    assert_eq!(
        started["thread_summary_applied"]["token_estimate"], 3,
        "{started}"
    );
}

#[tokio::test]
async fn openapi_declares_turn_mutations() {
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
    let turn = &doc["paths"]["/mini-chat/v1/chats/{id}/turns/{request_id}"];
    assert_eq!(
        turn["patch"]["operationId"], "mini_chat.edit_turn",
        "{turn}"
    );
    assert_eq!(
        turn["delete"]["operationId"], "mini_chat.delete_turn",
        "{turn}"
    );
    assert!(turn["delete"]["responses"]["204"].is_object(), "{turn}");
    assert!(
        turn["patch"]["responses"]["200"]["content"]["text/event-stream"].is_object(),
        "{turn}"
    );
    let retry = &doc["paths"]["/mini-chat/v1/chats/{id}/turns/{request_id}/retry"]["post"];
    assert_eq!(retry["operationId"], "mini_chat.retry_turn", "{retry}");
    assert!(
        retry["responses"]["200"]["content"]["text/event-stream"].is_object(),
        "{retry}"
    );
    assert_eq!(
        doc["components"]["schemas"]["EditTurnRequest"]["required"],
        json!(["content"])
    );
}
