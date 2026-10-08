//! Integration tests of the streaming pipeline through the router (acceptance criteria:
//! streaming, SSE contract, idempotency, parallel turns, lifecycle, preflight, settlement,
//! error mapping, web search, citations, context / request shape).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use http::StatusCode;
use mini_chat_sdk::TierLimits;
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::infra::db::entities::{attachment, chat, chat_turn, chat_vector_store, message, quota_usage};
use crate::infra::llm::transport::TransportError;
use crate::testing::{
    STANDARD, StreamScript, TestApp, USER_A1, USER_A2, completed_event, ctx, ctx_a1, ev, event_names, text_stream, TENANT_A,
    PREMIUM, NO_VISION, TINY,
};

pub(crate) async fn turns(t: &TestApp, chat_id: Uuid) -> Vec<chat_turn::Model> {
    let conn = t.app.db.conn().unwrap();
    chat_turn::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(chat_turn::Column::ChatId.eq(chat_id)))
        .all(&conn)
        .await
        .unwrap()
}

pub(crate) async fn messages(t: &TestApp, chat_id: Uuid) -> Vec<message::Model> {
    let conn = t.app.db.conn().unwrap();
    let mut v = message::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(message::Column::ChatId.eq(chat_id)))
        .all(&conn)
        .await
        .unwrap();
    v.sort_by_key(|m| (m.created_at, m.id));
    v
}

pub(crate) async fn quota_rows(t: &TestApp, user: Uuid) -> Vec<quota_usage::Model> {
    let conn = t.app.db.conn().unwrap();
    quota_usage::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(quota_usage::Column::UserId.eq(user)))
        .all(&conn)
        .await
        .unwrap()
}

pub(crate) async fn get_chat(t: &TestApp, chat_id: Uuid) -> chat::Model {
    let conn = t.app.db.conn().unwrap();
    chat::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
        .one(&conn)
        .await
        .unwrap()
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_attachment(
    t: &TestApp,
    chat_id: Uuid,
    user: Uuid,
    kind: &str,
    status: &str,
    file_id: Option<&str>,
    for_file_search: bool,
    for_code_interpreter: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    let now = crate::clock::now();
    let am = attachment::ActiveModel {
        id: Set(id),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(user),
        filename: Set(format!("{kind}-{id}.bin")),
        content_type: Set(if kind == "image" { "image/png".into() } else { "application/pdf".into() }),
        size_bytes: Set(10),
        storage_backend: Set("openai".into()),
        provider_file_id: Set(file_id.map(str::to_owned)),
        status: Set(status.into()),
        error_code: Set(None),
        attachment_kind: Set(kind.into()),
        for_file_search: Set(for_file_search),
        for_code_interpreter: Set(for_code_interpreter),
        doc_summary: Set(None),
        img_thumbnail: Set(None),
        img_thumbnail_width: Set(None),
        img_thumbnail_height: Set(None),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(None),
        cleanup_attempts: Set(0),
        last_cleanup_error: Set(None),
        cleanup_updated_at: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
        secondary_file_id: Set(None),
        secondary_status: Set("not_attempted".into()),
        secondary_provider_kind: Set(None),
    };
    let conn = t.app.db.conn().unwrap();
    secure_insert::<attachment::Entity>(am, &AccessScope::allow_all(), &conn).await.unwrap();
    id
}

pub(crate) async fn insert_vector_store(t: &TestApp, chat_id: Uuid, vs: &str) {
    let am = chat_vector_store::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        vector_store_id: Set(Some(vs.to_owned())),
        provider: Set("openai".into()),
        file_count: Set(0),
        created_at: Set(crate::clock::now()),
    };
    let conn = t.app.db.conn().unwrap();
    secure_insert::<chat_vector_store::Entity>(am, &AccessScope::allow_all(), &conn).await.unwrap();
}

fn data_of<'a>(events: &'a [crate::testing::SseEvent], name: &str) -> Vec<&'a serde_json::Value> {
    events.iter().filter(|e| e.event == name).map(|e| &e.data).collect()
}

// ───────────────────────────── basic streaming ─────────────────────────────

#[tokio::test]
async fn send_streams_events_in_order_and_persists_turn() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let before = get_chat(&t, chat_id).await.updated_at;
    let (st, events, err) = t.send(&c, chat_id, serde_json::json!({"content": "Hi there"})).await;
    assert_eq!(st, StatusCode::OK, "{err}");
    assert_eq!(event_names(&events), vec!["stream_started", "delta", "delta", "done"]);

    let started = &events[0].data;
    assert_eq!(started["is_new_turn"], true);
    let request_id: Uuid = started["request_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(request_id.get_version_num(), 4, "server generates a v4 request id");
    let message_id: Uuid = started["message_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(events[1].data, serde_json::json!({"type": "text", "content": "Hello"}));

    let done = &events[3].data;
    assert_eq!(done["usage"], serde_json::json!({"input_tokens": 10, "output_tokens": 5}));
    assert_eq!(done["effective_model"], STANDARD);
    assert_eq!(done["selected_model"], STANDARD);
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_from").is_none());
    assert!(done["quota_warnings"].is_array());
    assert!(done.get("request_id").is_none() && done.get("message_id").is_none());

    // Persistence: user + assistant message share the request id; turn completed.
    let msgs = messages(&t, chat_id).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[1].role, "assistant");
    assert_eq!(msgs[1].id, message_id);
    assert_eq!(msgs[1].content, "Hello world");
    assert_eq!(msgs[1].model.as_deref(), Some(STANDARD));
    assert_eq!((msgs[1].input_tokens, msgs[1].output_tokens), (10, 5));
    assert!(msgs.iter().all(|m| m.request_id == Some(request_id)));
    let ts = turns(&t, chat_id).await;
    assert_eq!(ts.len(), 1);
    assert_eq!(ts[0].state, "completed");
    assert_eq!(ts[0].assistant_message_id, Some(message_id));
    assert!(ts[0].completed_at.is_some());
    assert_eq!(ts[0].effective_model.as_deref(), Some(STANDARD));
    assert!(ts[0].reserve_tokens.unwrap() > 0);
    assert_eq!(ts[0].policy_version_applied, Some(1));
    assert!(get_chat(&t, chat_id).await.updated_at > before, "send bumps chats.updated_at");
}

#[tokio::test]
async fn client_request_id_of_any_version_is_used() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let rid = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001);
    let (st, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "x", "request_id": rid})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(events[0].data["request_id"], rid.to_string());
}

#[tokio::test]
async fn completed_turn_settles_quota_and_publishes_usage_once() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let (st, _, _) = t.send(&c, chat_id, serde_json::json!({"content": "Hi"})).await;
    assert_eq!(st, StatusCode::OK);
    // standard: in 1_000_000, out 3_000_000 per 1M tokens → ceil(10*1)+ceil(5*3) = 10 + 15 = 25.
    let rows = quota_rows(&t, USER_A1).await;
    let totals: Vec<_> = rows.iter().filter(|r| r.bucket == "total").collect();
    assert_eq!(totals.len(), 2, "daily + monthly total rows");
    for r in &totals {
        assert_eq!(r.spent_credits_micro, 25);
        assert_eq!(r.reserved_credits_micro, 0, "reserve released");
        assert_eq!(r.calls, 1);
        assert_eq!((r.input_tokens, r.output_tokens), (10, 5));
    }
    assert!(rows.iter().all(|r| r.bucket != "tier:premium"), "standard turn does not touch the premium bucket");
    t.eventually("usage published", || async { t.policy.published.lock().unwrap().len() == 1 }).await;
    let ev = t.policy.published.lock().unwrap()[0].clone();
    assert_eq!(ev.billing_outcome, "completed");
    assert_eq!(ev.settlement_method, "actual");
    assert_eq!(ev.actual_credits_micro, 25);
    let turn = &turns(&t, chat_id).await[0];
    assert_eq!(ev.dedupe_key, format!("{}/{}/{}", TENANT_A.simple(), turn.id.simple(), turn.request_id.simple()));
    t.eventually("audit delivered", || async {
        t.audit.events.lock().unwrap().iter().any(|e| e.event_type() == "turn_completed")
    })
    .await;
}

#[tokio::test]
async fn premium_turn_charges_both_buckets() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(PREMIUM)).await;
    let (st, _, _) = t.send(&c, chat_id, serde_json::json!({"content": "Hi"})).await;
    assert_eq!(st, StatusCode::OK);
    let rows = quota_rows(&t, USER_A1).await;
    // premium: in 3_000_000, out 15_000_000 → 30 + 75 = 105.
    assert_eq!(rows.iter().filter(|r| r.bucket == "tier:premium" && r.spent_credits_micro == 105).count(), 2);
    assert_eq!(rows.iter().filter(|r| r.bucket == "total" && r.spent_credits_micro == 105).count(), 2);
}

#[tokio::test]
async fn sse_payloads_never_expose_provider_ids() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let (_, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "Hi"})).await;
    for e in &events {
        let s = e.data.to_string();
        assert!(!s.contains("resp_"), "{s}");
    }
}

// ───────────────────────────── errors ─────────────────────────────

#[tokio::test]
async fn provider_failure_is_terminal_sanitized_error() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_events(vec![
        ev("response.output_text.delta", serde_json::json!({"delta": "par"})),
        ev(
            "response.failed",
            serde_json::json!({"response": {"id": "resp_x1", "status": "failed",
                "error": {"code": "server_error", "message": "boom resp_abcdef123 at https://internal.example/x key sk-abcdefghijklmnop"}}}),
        ),
    ]);
    let (st, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "Hi"})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(event_names(&events), vec!["stream_started", "delta", "error"]);
    let err = &events[2].data;
    assert_eq!(err["code"], "provider_error");
    let msg = err["message"].as_str().unwrap();
    assert!(!msg.contains("resp_abcdef123") && !msg.contains("https://") && !msg.contains("sk-abc"), "{msg}");
    assert!(msg.contains("[provider_id]") && msg.contains("[url]"), "{msg}");
    let turn = &turns(&t, chat_id).await[0];
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert!(turn.assistant_message_id.is_none());
    // Failed without usage → estimated settlement.
    t.eventually("usage published", || async { !t.policy.published.lock().unwrap().is_empty() }).await;
    let ev = t.policy.published.lock().unwrap()[0].clone();
    assert_eq!((ev.billing_outcome.as_str(), ev.settlement_method.as_str()), ("failed", "estimated"));
    assert!(ev.actual_credits_micro > 0);
    let (_, _, status) = t
        .call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/turns/{}", turn.request_id), None)
        .await;
    assert_eq!(status["state"], "error");
    assert_eq!(status["error_code"], "provider_error");
    assert!(status.get("assistant_message_id").is_none());
}

#[tokio::test]
async fn provider_http_429_maps_to_rate_limited() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_stream(StreamScript::Http {
        status: 429,
        body: serde_json::json!({"error": {"message": "slow down"}}),
        retry_after: Some(7),
    });
    let (st, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "Hi"})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(event_names(&events), vec!["stream_started", "error"]);
    assert_eq!(events[1].data["code"], "rate_limited");
    assert_eq!(turns(&t, chat_id).await[0].error_code.as_deref(), Some("rate_limited"));
}

#[tokio::test]
async fn transport_timeout_maps_to_provider_timeout() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_stream(StreamScript::Transport(TransportError::Timeout("gw".into())));
    let (_, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "Hi"})).await;
    assert_eq!(events.last().unwrap().data["code"], "provider_timeout");
}

#[tokio::test]
async fn provider_http_500_maps_to_provider_error() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_stream(StreamScript::Http {
        status: 500,
        body: serde_json::json!({"error": {"message": "internal file-abcdefghijklmnop"}}),
        retry_after: None,
    });
    let (_, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "Hi"})).await;
    let e = &events.last().unwrap().data;
    assert_eq!(e["code"], "provider_error");
    assert!(!e["message"].as_str().unwrap().contains("file-abcdefghijklmnop"));
}

// ───────────────────────────── preflight ─────────────────────────────

#[tokio::test]
async fn empty_content_is_rejected_before_provider() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let (st, events, err) = t.send(&c, chat_id, serde_json::json!({"content": "   "})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(events.is_empty());
    assert_eq!(err["context"]["field_violations"][0]["reason"], "EMPTY_CONTENT");
    assert!(t.provider.chat_bodies().is_empty());
    assert!(turns(&t, chat_id).await.is_empty());
}

#[tokio::test]
async fn malformed_body_and_unknown_chat() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let (st, _, _) = t.send(&c, chat_id, serde_json::json!({"no_content": 1})).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _, _) = t.send(&c, chat_id, serde_json::json!({"content": "x", "attachment_ids": ["nope"]})).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _, err) = t.send(&c, Uuid::new_v4(), serde_json::json!({"content": "x"})).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(err["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
}

#[tokio::test]
async fn foreign_users_cannot_send() {
    let t = TestApp::new().await;
    let chat_id = t.create_chat(&ctx_a1(), Some(STANDARD)).await;
    for other in [ctx(USER_A2, TENANT_A), ctx(crate::testing::USER_B1, crate::testing::TENANT_B)] {
        let (st, _, _) = t.send(&other, chat_id, serde_json::json!({"content": "x"})).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
    assert!(t.provider.chat_bodies().is_empty());
}

#[tokio::test]
async fn removed_chat_model_is_invalid_model() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.policy.with_snapshot(|s| s.model_catalog.retain(|m| m.id != STANDARD));
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "x"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(err["context"]["field_violations"][0]["reason"], "INVALID_MODEL");
}

#[tokio::test]
async fn web_search_kill_switch_rejects() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.policy.with_snapshot(|s| s.kill_switches.disable_web_search = true);
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "x", "web_search": {"enabled": true}})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(err["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(err["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    // Without the flag the request is fine.
    let (st, _, _) = t.send(&c, chat_id, serde_json::json!({"content": "x"})).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn quota_exhausted_rejects_with_429_before_provider() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.policy.set_limits(
        TierLimits { limit_daily_credits_micro: 1, limit_monthly_credits_micro: 1 },
        TierLimits { limit_daily_credits_micro: 1, limit_monthly_credits_micro: 1 },
    );
    let (st, events, err) = t.send(&c, chat_id, serde_json::json!({"content": "x"})).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{err}");
    assert!(events.is_empty());
    assert_eq!(err["context"]["violations"][0]["subject"], "tokens");
    assert!(t.provider.chat_bodies().is_empty());
    assert!(turns(&t, chat_id).await.is_empty());
    assert!(messages(&t, chat_id).await.is_empty());
}

#[tokio::test]
async fn premium_exhausted_downgrades_to_standard() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(PREMIUM)).await;
    t.policy.set_limits(
        TierLimits { limit_daily_credits_micro: 100_000_000, limit_monthly_credits_micro: 1_000_000_000 },
        TierLimits { limit_daily_credits_micro: 1, limit_monthly_credits_micro: 500_000_000 },
    );
    let (st, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "x"})).await;
    assert_eq!(st, StatusCode::OK);
    let done = data_of(&events, "done")[0];
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["selected_model"], PREMIUM);
    assert_eq!(done["effective_model"], STANDARD);
    assert_eq!(done["downgrade_from"], PREMIUM);
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");
    let msgs = messages(&t, chat_id).await;
    assert_eq!(msgs[1].model.as_deref(), Some(STANDARD));
    assert_eq!(get_chat(&t, chat_id).await.model, PREMIUM, "selected model is immutable");
    assert_eq!(t.provider.chat_bodies()[0]["model"], format!("{STANDARD}-provider"));
}

#[tokio::test]
async fn image_guards() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    // Vision not supported by the effective model.
    let chat_id = t.create_chat(&c, Some(NO_VISION)).await;
    let img = insert_attachment(&t, chat_id, USER_A1, "image", "ready", Some("file-img0000000000001"), false, false).await;
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "what?", "attachment_ids": [img]})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(err["context"]["field_violations"][0]["reason"], "VISION_NOT_SUPPORTED");

    // Too many images.
    let chat2 = t.create_chat(&c, Some(STANDARD)).await;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(insert_attachment(&t, chat2, USER_A1, "image", "ready", Some(&format!("file-img00000000000{i}")), false, false).await);
    }
    let (st, _, err) = t.send(&c, chat2, serde_json::json!({"content": "x", "attachment_ids": ids})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(err["context"]["field_violations"][0]["reason"], "TOO_MANY_IMAGES");

    // Kill switch.
    t.policy.with_snapshot(|s| s.kill_switches.disable_images = true);
    let (st, _, err) = t.send(&c, chat2, serde_json::json!({"content": "x", "attachment_ids": [ids[0]]})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(err["context"]["violations"][0]["subject"], "images");
    assert!(t.provider.chat_bodies().is_empty());
}

#[tokio::test]
async fn image_is_sent_as_input_image_and_linked() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let img = insert_attachment(&t, chat_id, USER_A1, "image", "ready", Some("file-img0000000000042"), false, false).await;
    let (st, _, _) = t.send(&c, chat_id, serde_json::json!({"content": "describe", "attachment_ids": [img]})).await;
    assert_eq!(st, StatusCode::OK);
    let body = &t.provider.chat_bodies()[0];
    let s = body["input"].to_string();
    assert!(s.contains("input_image") && s.contains("file-img0000000000042"), "{s}");
    let (_, _, list) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/messages"), None).await;
    assert_eq!(list["items"][0]["attachments"][0]["attachment_id"], img.to_string());
}

#[tokio::test]
async fn invalid_attachment_ids_roll_back_everything() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let other_chat = t.create_chat(&c, Some(STANDARD)).await;
    let foreign = insert_attachment(&t, other_chat, USER_A1, "document", "ready", Some("file-doc0000000000001"), true, false).await;
    let pending = insert_attachment(&t, chat_id, USER_A1, "document", "pending", None, true, false).await;
    let ok = insert_attachment(&t, chat_id, USER_A1, "document", "ready", Some("file-doc0000000000002"), true, false).await;
    for ids in [vec![foreign], vec![pending], vec![ok, ok], vec![Uuid::new_v4()]] {
        let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "x", "attachment_ids": ids})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");
        assert_eq!(err["context"]["field_violations"][0]["reason"], "invalid_attachment");
    }
    assert!(turns(&t, chat_id).await.is_empty());
    assert!(messages(&t, chat_id).await.is_empty());
    for r in quota_rows(&t, USER_A1).await {
        assert_eq!(r.reserved_credits_micro, 0, "no reserve survives a rejected request");
    }
}

#[tokio::test]
async fn input_too_long_is_rejected() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(TINY)).await;
    let long = "word ".repeat(4000);
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": long})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(err["context"]["field_violations"][0]["reason"], "INPUT_TOO_LONG");
}

#[tokio::test]
async fn mandatory_context_over_budget_is_rejected() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(TINY)).await;
    t.policy.with_snapshot(|s| {
        for m in &mut s.model_catalog {
            if m.id == TINY {
                m.system_prompt = "rule ".repeat(4000);
            }
        }
    });
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "hello"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(err["context"]["field_violations"][0]["reason"], "CONTEXT_BUDGET_EXCEEDED");
    assert!(turns(&t, chat_id).await.is_empty());
}

// ───────────────────────────── idempotency & parallel turns ─────────────────────────────

#[tokio::test]
async fn replay_of_completed_turn_is_side_effect_free() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let rid = Uuid::new_v4();
    let (_, first, _) = t.send(&c, chat_id, serde_json::json!({"content": "Hi", "request_id": rid})).await;
    t.eventually("usage published", || async { t.policy.published.lock().unwrap().len() == 1 }).await;
    let rows_before = quota_rows(&t, USER_A1).await;
    let (st, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "Hi", "request_id": rid})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(event_names(&events), vec!["stream_started", "delta", "done"]);
    assert_eq!(events[0].data["is_new_turn"], false);
    assert_eq!(events[0].data["request_id"], rid.to_string());
    let assistant = messages(&t, chat_id).await.into_iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(events[0].data["message_id"], assistant.id.to_string());
    assert_eq!(events[1].data["content"], "Hello world");
    let done = &events[2].data;
    assert_eq!(done["usage"], data_of(&first, "done")[0]["usage"]);
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("quota_warnings").is_none());
    assert!(done.get("downgrade_reason").is_none());
    assert_eq!(t.provider.chat_bodies().len(), 1, "no provider call on replay");
    assert_eq!(quota_rows(&t, USER_A1).await, rows_before, "no quota change on replay");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(t.policy.published.lock().unwrap().len(), 1, "no new usage event on replay");
    assert_eq!(turns(&t, chat_id).await.len(), 1);
}

#[tokio::test]
async fn request_id_of_failed_turn_conflicts() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_stream(StreamScript::Http { status: 500, body: serde_json::json!({}), retry_after: None });
    let rid = Uuid::new_v4();
    let _ = t.send(&c, chat_id, serde_json::json!({"content": "Hi", "request_id": rid})).await;
    let (st, events, err) = t.send(&c, chat_id, serde_json::json!({"content": "Hi", "request_id": rid})).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(events.is_empty());
    assert_eq!(err["context"]["reason"], "request_id_conflict");
}

/// Starts a hanging turn in the background; returns the request id once its turn row exists.
async fn start_hanging_turn(t: &TestApp, chat_id: Uuid) -> (Uuid, tokio::task::JoinHandle<()>) {
    t.provider.push_stream(StreamScript::EventsThenHang {
        events: vec![ev("response.output_text.delta", serde_json::json!({"delta": "partial"}))],
        delay: Duration::ZERO,
    });
    let rid = Uuid::new_v4();
    let router = t.router();
    let c = ctx_a1();
    let handle = tokio::spawn(async move {
        let mut req = http::Request::builder()
            .method("POST")
            .uri(format!("/mini-chat/v1/chats/{chat_id}/messages:stream"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::json!({"content": "long", "request_id": rid}).to_string()))
            .unwrap();
        req.extensions_mut().insert(c);
        let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
        // Keep the body open (client connected) until the task is aborted.
        let mut body = resp.into_body();
        while let Some(f) = http_body_util::BodyExt::frame(&mut body).await {
            if f.is_err() {
                break;
            }
        }
    });
    t.eventually("turn running", || async { turns(t, chat_id).await.iter().any(|x| x.request_id == rid) }).await;
    (rid, handle)
}

#[tokio::test]
async fn only_one_running_turn_per_chat() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let (rid, handle) = start_hanging_turn(&t, chat_id).await;
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "second"})).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err["context"]["reason"], "turn_already_running");
    // Same request id while running → request_id_conflict (idempotency check comes first).
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "x", "request_id": rid})).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err["context"]["reason"], "request_id_conflict");
    let (_, _, status) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/turns/{rid}"), None).await;
    assert_eq!(status["state"], "running");

    // Disconnect: the turn is cancelled, partial content persisted, provider stream dropped.
    handle.abort();
    t.eventually("turn cancelled", || async { turns(&t, chat_id).await[0].state == "cancelled" }).await;
    t.eventually("provider stream dropped", || async { t.provider.dropped_streams.load(std::sync::atomic::Ordering::SeqCst) >= 1 }).await;
    let turn = &turns(&t, chat_id).await[0];
    let msg_id = turn.assistant_message_id.expect("partial content persisted");
    let msgs = messages(&t, chat_id).await;
    assert_eq!(msgs.iter().find(|m| m.id == msg_id).unwrap().content, "partial");
    let (_, _, status) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/turns/{rid}"), None).await;
    assert_eq!(status["state"], "cancelled");
    assert_eq!(status["assistant_message_id"], msg_id.to_string());
    // Aborted settlement: estimated, reserve released.
    t.eventually("usage published", || async {
        t.policy.published.lock().unwrap().iter().any(|e| e.billing_outcome == "aborted" && e.settlement_method == "estimated")
    })
    .await;
    for r in quota_rows(&t, USER_A1).await {
        assert_eq!(r.reserved_credits_micro, 0);
        assert!(r.spent_credits_micro > 0, "cancellation cannot evade quota");
    }

    // A new turn is accepted once the previous one is terminal.
    let (st, _, _) = t.send(&c, chat_id, serde_json::json!({"content": "again"})).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn cancel_before_content_persists_no_message() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_stream(StreamScript::EventsThenHang { events: vec![], delay: Duration::ZERO });
    let router = t.router();
    let c2 = c.clone();
    let handle = tokio::spawn(async move {
        let mut req = http::Request::builder()
            .method("POST")
            .uri(format!("/mini-chat/v1/chats/{chat_id}/messages:stream"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::json!({"content": "x"}).to_string()))
            .unwrap();
        req.extensions_mut().insert(c2);
        let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
        let mut body = resp.into_body();
        while http_body_util::BodyExt::frame(&mut body).await.is_some() {}
    });
    t.eventually("turn running", || async { !turns(&t, chat_id).await.is_empty() }).await;
    handle.abort();
    t.eventually("cancelled", || async { turns(&t, chat_id).await[0].state == "cancelled" }).await;
    let turn = &turns(&t, chat_id).await[0];
    assert!(turn.assistant_message_id.is_none());
    assert_eq!(messages(&t, chat_id).await.len(), 1, "only the user message");
}

#[tokio::test]
async fn ping_is_sent_before_first_content() {
    let t = TestApp::with_config(|cfg| cfg.streaming.sse_ping_interval_seconds = 5).await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_stream(StreamScript::Events { events: text_stream(&["late"], 1, 1), delay: Duration::from_millis(5600) });
    let (st, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "x"})).await;
    assert_eq!(st, StatusCode::OK);
    let names = event_names(&events);
    assert_eq!(names[0], "stream_started");
    assert_eq!(names[1], "ping");
    assert_eq!(events[1].data, serde_json::json!({}));
    let first_delta = names.iter().position(|n| n == "delta").unwrap();
    assert!(names[first_delta..].iter().all(|n| n != "ping"), "no ping after content: {names:?}");
    assert_eq!(names.last().unwrap(), "done");
}

// ───────────────────────────── tools, citations, request shape ─────────────────────────────

#[tokio::test]
async fn web_search_tool_events_counts_and_citations() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let annotation = serde_json::json!({"annotation": {"type": "url_citation", "url": "https://example.com/a", "title": "A", "start_index": 0, "end_index": 5}, "output_index": 1, "content_index": 0});
    t.provider.push_events(vec![
        ev("response.web_search_call.searching", serde_json::json!({"item_id": "ws_1"})),
        ev("response.web_search_call.completed", serde_json::json!({"item_id": "ws_1"})),
        ev("response.output_text.delta", serde_json::json!({"delta": "Hello world", "output_index": 1, "content_index": 0})),
        ev("response.output_text.annotation.added", annotation),
        completed_event(20, 10),
    ]);
    let (st, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "news?", "web_search": {"enabled": true}})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(event_names(&events), vec!["stream_started", "tool", "tool", "delta", "citations", "done"]);
    assert_eq!(events[1].data, serde_json::json!({"phase": "start", "name": "web_search", "details": {}}));
    assert_eq!(events[2].data["phase"], "done");
    let item = &events[4].data["items"][0];
    assert_eq!(item["source"], "web");
    assert_eq!(item["url"], "https://example.com/a");
    assert_eq!(item["snippet"], "Hello");
    assert_eq!(item["span"], serde_json::json!({"start": 0, "end": 5}));
    // Request carries the web_search tool and the guard.
    let body = &t.provider.chat_bodies()[0];
    assert!(body["tools"].to_string().contains("web_search"));
    assert!(body["instructions"].as_str().unwrap().contains("Use web_search only if"));
    assert_eq!(body["metadata"]["feature"], "web_search");
    // Counted on the turn and in quota usage.
    let turn = &turns(&t, chat_id).await[0];
    assert_eq!(turn.web_search_completed_count, 1);
    assert!(turn.web_search_enabled);
    let daily_total = quota_rows(&t, USER_A1).await.into_iter().find(|r| r.bucket == "total" && r.period_type == "daily").unwrap();
    assert_eq!(daily_total.web_search_calls, 1);
}

#[tokio::test]
async fn web_search_per_message_limit_fails_turn() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_events(vec![
        ev("response.web_search_call.searching", serde_json::json!({})),
        ev("response.web_search_call.completed", serde_json::json!({})),
        ev("response.web_search_call.searching", serde_json::json!({})),
        ev("response.web_search_call.completed", serde_json::json!({})),
        ev("response.web_search_call.searching", serde_json::json!({})),
        completed_event(1, 1),
    ]);
    let (_, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "x", "web_search": {"enabled": true}})).await;
    let last = events.last().unwrap();
    assert_eq!(last.event, "error");
    assert_eq!(last.data["code"], "web_search_calls_exceeded");
    let turn = &turns(&t, chat_id).await[0];
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("web_search_calls_exceeded"));
}

#[tokio::test]
async fn web_search_daily_quota_is_checked_only_when_enabled() {
    let t = TestApp::with_config(|cfg| cfg.quota.web_search_daily_quota = 1).await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_events(vec![
        ev("response.web_search_call.searching", serde_json::json!({})),
        ev("response.web_search_call.completed", serde_json::json!({})),
        completed_event(1, 1),
    ]);
    let (st, _, _) = t.send(&c, chat_id, serde_json::json!({"content": "x", "web_search": {"enabled": true}})).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "x", "web_search": {"enabled": true}})).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(err["context"]["violations"][0]["subject"], "web_search");
    let (st, _, _) = t.send(&c, chat_id, serde_json::json!({"content": "x"})).await;
    assert_eq!(st, StatusCode::OK, "requests without web search are not affected");
}

#[tokio::test]
async fn file_search_and_code_interpreter_tools_from_ready_attachments() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let doc = insert_attachment(&t, chat_id, USER_A1, "document", "ready", Some("file-doc0000000000077"), true, false).await;
    insert_attachment(&t, chat_id, USER_A1, "document", "ready", Some("file-xls0000000000088"), false, true).await;
    insert_vector_store(&t, chat_id, "vs_chatstore00000001").await;
    t.provider.push_events(vec![
        ev("response.file_search_call.searching", serde_json::json!({})),
        ev("response.file_search_call.completed", serde_json::json!({})),
        ev("response.output_text.delta", serde_json::json!({"delta": "From the doc"})),
        ev(
            "response.output_text.annotation.added",
            serde_json::json!({"annotation": {"type": "file_citation", "file_id": "file-doc0000000000077", "filename": "x.pdf", "index": 3}}),
        ),
        ev(
            "response.output_text.annotation.added",
            serde_json::json!({"annotation": {"type": "file_citation", "file_id": "file-unknown00000000099", "filename": "y.pdf", "index": 3}}),
        ),
        completed_event(5, 5),
    ]);
    let (st, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "summarize"})).await;
    assert_eq!(st, StatusCode::OK);
    let body = &t.provider.chat_bodies()[0];
    let tools = body["tools"].as_array().unwrap();
    let fs = tools.iter().find(|x| x["type"] == "file_search").unwrap();
    assert_eq!(fs["vector_store_ids"], serde_json::json!(["vs_chatstore00000001"]));
    let ci = tools.iter().find(|x| x["type"] == "code_interpreter").unwrap();
    assert_eq!(ci["container"]["file_ids"], serde_json::json!(["file-xls0000000000088"]));
    assert_eq!(body["include"], serde_json::json!(["code_interpreter_call.outputs"]));
    let cit = data_of(&events, "citations");
    assert_eq!(cit.len(), 1);
    let items = cit[0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "unknown provider file ids are omitted");
    assert_eq!(items[0]["source"], "file");
    assert_eq!(items[0]["attachment_id"], doc.to_string());
    assert_eq!(items[0]["snippet"], "");
    assert!(!cit[0].to_string().contains("file-doc"), "provider file id never exposed");
    assert_eq!(turns(&t, chat_id).await[0].file_search_completed_count, 1);
}

#[tokio::test]
async fn no_tools_without_attachments_or_web_search() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let _ = t.send(&c, chat_id, serde_json::json!({"content": "x"})).await;
    let body = &t.provider.chat_bodies()[0];
    assert!(body.get("tools").is_none());
    assert_eq!(body["metadata"]["request_type"], "chat");
    assert_eq!(body["metadata"]["feature"], "none");
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], format!("You are {STANDARD}."));
}

#[tokio::test]
async fn history_is_included_in_order() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let _ = t.send(&c, chat_id, serde_json::json!({"content": "first question"})).await;
    let _ = t.send(&c, chat_id, serde_json::json!({"content": "second question"})).await;
    let body = &t.provider.chat_bodies()[1];
    let input = body["input"].as_array().unwrap();
    let texts: Vec<String> = input.iter().map(|i| i["content"][0]["text"].as_str().unwrap_or_default().to_owned()).collect();
    assert_eq!(texts, vec!["first question", "Hello world", "second question"]);
    assert_eq!(input[1]["role"], "assistant");
    // message_count and chronological listing.
    let (_, _, chat) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}"), None).await;
    assert_eq!(chat["message_count"], 4);
    let (_, _, list) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/messages"), None).await;
    let roles: Vec<&str> = list["items"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
}

#[tokio::test]
async fn turn_status_endpoint() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let (_, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "x"})).await;
    let rid = events[0].data["request_id"].as_str().unwrap().to_owned();
    let (st, _, status) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/turns/{rid}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(status["state"], "done");
    assert_eq!(status["request_id"], rid);
    assert_eq!(status["assistant_message_id"], events[0].data["message_id"]);
    assert!(status.get("error_code").is_none());
    let (st, _, err) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/turns/{}", Uuid::new_v4()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(err["context"]["resource_type"], "gts.cf.core.mini_chat.turn.v1~");
    let (st, _, _) = t.call(&ctx(USER_A2, TENANT_A), "GET", &format!("/mini-chat/v1/chats/{chat_id}/turns/{rid}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, _) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/turns/not-a-uuid"), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn authz_failures_map_to_403_and_503() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.authz.deny.store(true, std::sync::atomic::Ordering::SeqCst);
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "x"})).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(err["context"]["reason"], "AUTHZ_DENIED");
    t.authz.deny.store(false, std::sync::atomic::Ordering::SeqCst);
    t.authz.unavailable.store(true, std::sync::atomic::Ordering::SeqCst);
    let (st, headers, _) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/turns/{}", Uuid::new_v4()), None).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers.get("retry-after").unwrap(), "5");
}

#[tokio::test]
async fn thread_summary_applied_is_reported() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let _ = t.send(&c, chat_id, serde_json::json!({"content": "first"})).await;
    let msgs = messages(&t, chat_id).await;
    let now = crate::clock::now();
    let am = crate::infra::db::entities::thread_summary::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        summary_text: Set("The user said first.".into()),
        summarized_up_to_created_at: Set(msgs[1].created_at),
        summarized_up_to_message_id: Set(msgs[1].id),
        token_estimate: Set(42),
        created_at: Set(now),
        updated_at: Set(now),
    };
    let conn = t.app.db.conn().unwrap();
    secure_insert::<crate::infra::db::entities::thread_summary::Entity>(am, &AccessScope::allow_all(), &conn).await.unwrap();
    drop(conn);
    let (_, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "second"})).await;
    assert_eq!(events[0].data["thread_summary_applied"], serde_json::json!({"token_estimate": 42}));
    let body = &t.provider.chat_bodies()[1];
    let first = body["input"][0]["content"][0]["text"].as_str().unwrap();
    assert!(first.contains("The user said first."), "{first}");
    // Summarized messages are not re-sent.
    assert!(!body["input"].to_string().contains("\"first\""));
}

#[tokio::test]
async fn deltas_are_relayed_before_the_provider_finishes() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    // The provider sends one delta and then keeps the stream open: the delta must reach the
    // client while the turn is still running (no buffering of the response).
    t.provider.push_stream(StreamScript::EventsThenHang {
        events: vec![ev("response.output_text.delta", serde_json::json!({"delta": "first chunk"}))],
        delay: Duration::ZERO,
    });
    let mut req = http::Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat_id}/messages:stream"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::json!({"content": "x"}).to_string()))
        .unwrap();
    req.extensions_mut().insert(c.clone());
    let resp = tower::ServiceExt::oneshot(t.router(), req).await.unwrap();
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/event-stream");
    assert_eq!(resp.headers().get("cache-control").unwrap(), "no-cache");
    let mut body = resp.into_body();
    let mut seen = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !seen.contains("first chunk") {
            let frame = http_body_util::BodyExt::frame(&mut body).await.unwrap().unwrap();
            if let Ok(data) = frame.into_data() {
                seen.push_str(&String::from_utf8_lossy(&data));
            }
        }
    })
    .await
    .expect("delta must be relayed while the provider stream is still open");
    assert_eq!(turns(&t, chat_id).await[0].state, "running");
    drop(body);
    t.eventually("cancelled after disconnect", || async { turns(&t, chat_id).await[0].state == "cancelled" }).await;
}
