//! Router tests: idempotent replay, request-id conflicts, parallel turn guard,
//! turn status API, client-disconnect cancellation and the orphan watchdog
//! (acceptance: Idempotency & Replay, Parallel Turn Enforcement, Turn Lifecycle,
//! Settlement & Finalization).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;

use serde_json::json;
use uuid::Uuid;

use crate::infra::llm::sse_parser::SseParser;
use crate::test_support::{
    TENANT_A, TestEnv, USER_A2, ctx, json_resp, live_provider, next_event, sse_chunk, user_a,
};


fn stream_uri(chat: &str) -> String {
    format!("/mini-chat/v1/chats/{chat}/messages:stream")
}

async fn turn_row(env: &TestEnv, rid: Uuid) -> (String, Option<String>, Option<Vec<u8>>) {
    let rows = env
        .sql_rows(&format!(
            "SELECT state, error_code, assistant_message_id FROM chat_turns WHERE request_id = x'{}'",
            rid.simple()
        ))
        .await;
    let r = &rows[0];
    (
        r.try_get_by_index::<String>(0).unwrap(),
        r.try_get_by_index::<Option<String>>(1).unwrap(),
        r.try_get_by_index::<Option<Vec<u8>>>(2).unwrap(),
    )
}

async fn wait_state(env: &TestEnv, rid: Uuid, state: &str) {
    for _ in 0..200 {
        if turn_row(env, rid).await.0 == state {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("turn {rid} never reached {state}");
}

async fn quota_snapshot(env: &TestEnv) -> Vec<(String, i64, i64, i64)> {
    env.sql_rows("SELECT bucket, spent_credits_micro, reserved_credits_micro, calls FROM quota_usage ORDER BY period_type, bucket")
        .await
        .iter()
        .map(|r| {
            (
                r.try_get_by_index::<String>(0).unwrap(),
                r.try_get_by_index::<i64>(1).unwrap(),
                r.try_get_by_index::<i64>(2).unwrap(),
                r.try_get_by_index::<i64>(3).unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn replay_returns_stored_result_without_side_effects() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let rid = Uuid::new_v4();
    let first = env.stream(&user_a(), &chat, json!({"content": "Hi", "request_id": rid})).await;
    assert_eq!(first.event_names().last().unwrap(), "done");
    let msg_id = first.event("stream_started").unwrap()["message_id"].clone();
    env.eventually("usage event", |e| e.usage_events().len() == 1).await;
    let quota_before = quota_snapshot(&env).await;
    assert!(quota_before.iter().any(|(_, spent, _, _)| *spent > 0));
    let provider_calls = env.proxy.chat_requests().len();

    let replay = env.stream(&user_a(), &chat, json!({"content": "different text", "request_id": rid})).await;
    assert_eq!(replay.status, 200);
    assert_eq!(replay.event_names(), vec!["stream_started", "delta", "done"]);
    let started = replay.event("stream_started").unwrap();
    assert_eq!(started["is_new_turn"], false);
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["message_id"], msg_id);
    let delta = replay.event("delta").unwrap();
    assert_eq!(delta, json!({"type": "text", "content": "Hello world"}));
    let done = replay.event("done").unwrap();
    assert_eq!(done["effective_model"], "gpt-4.1");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("quota_warnings").is_none(), "no warnings on replay");
    assert!(done.get("downgrade_reason").is_none());

    env.settle().await;
    assert_eq!(env.proxy.chat_requests().len(), provider_calls, "replay never calls the provider");
    assert_eq!(quota_snapshot(&env).await, quota_before, "replay changes no quota");
    assert_eq!(env.usage_events().len(), 1, "replay publishes no usage");
    assert_eq!(env.count("SELECT COUNT(*) FROM messages").await, 2);
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_turns").await, 1);
}

#[tokio::test]
async fn request_id_reuse_conflicts_and_parallel_guard() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;

    // failed turn → conflict
    let failed = Uuid::new_v4();
    let slot = std::sync::Mutex::new(Some(json_resp(500, &json!({"error": {"message": "boom"}}))));
    env.proxy.respond(move |r| if r.uri.contains("/responses") { slot.lock().unwrap().take() } else { None });
    let r = env.stream(&user_a(), &chat, json!({"content": "x", "request_id": failed})).await;
    assert_eq!(r.event_names().last().unwrap(), "error");
    let r = env.stream(&user_a(), &chat, json!({"content": "x", "request_id": failed})).await;
    r.assert_problem(409, "request_id_conflict");
    assert!(r.headers["content-type"].to_str().unwrap().contains("json"), "no SSE stream on conflict");

    // running turn: same id → request_id_conflict (replay check first), other ids → turn_already_running
    let tx = live_provider(&env.proxy);
    let running = Uuid::new_v4();
    let (status, mut body) = env.open(&user_a(), "POST", &stream_uri(&chat), json!({"content": "go", "request_id": running})).await;
    assert_eq!(status, 200);
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    let r = env.stream(&user_a(), &chat, json!({"content": "x", "request_id": running})).await;
    r.assert_problem(409, "request_id_conflict");
    let r = env.stream(&user_a(), &chat, json!({"content": "x", "request_id": Uuid::new_v4()})).await;
    r.assert_problem(409, "turn_already_running");
    let r = env.stream(&user_a(), &chat, json!({"content": "x"})).await;
    r.assert_problem(409, "turn_already_running");
    // a replay of a completed turn is still served while another turn runs? (no completed turn yet here)
    let calls_before = env.proxy.chat_requests().len();
    assert_eq!(calls_before, 2, "rejected requests never reach the provider");
    tx.send(sse_chunk("response.output_text.delta", &json!({"delta": "ok"}))).await.unwrap();
    tx.send(sse_chunk("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})))
        .await
        .unwrap();
    loop {
        let (name, _) = next_event(&mut body, &mut p, &mut q).await.unwrap();
        if name == "done" {
            break;
        }
    }
    wait_state(&env, running, "completed").await;

    // a new turn is accepted once the previous one is terminal
    let next = Uuid::new_v4();
    let r = env.stream(&user_a(), &chat, json!({"content": "next", "request_id": next})).await;
    assert_eq!(r.event_names().last().unwrap(), "done");

    // soft-deleted turn → conflict
    let r = env.call(&user_a(), "DELETE", &format!("/mini-chat/v1/chats/{chat}/turns/{next}"), None).await;
    assert_eq!(r.status, 204, "{}", r.text());
    let r = env.stream(&user_a(), &chat, json!({"content": "again", "request_id": next})).await;
    r.assert_problem(409, "request_id_conflict");
    assert_eq!(r.json()["context"]["reason"], "request_id_conflict");
    let detail = r.json()["detail"].as_str().unwrap_or_default().to_owned();
    assert!(!detail.contains(&next.to_string()), "generic detail only: {}", r.text());
}

#[tokio::test]
async fn replay_is_checked_before_parallel_guard() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let done_rid = Uuid::new_v4();
    let r = env.stream(&user_a(), &chat, json!({"content": "a", "request_id": done_rid})).await;
    assert_eq!(r.event_names().last().unwrap(), "done");
    let tx = live_provider(&env.proxy);
    let (_, mut body) = env.open(&user_a(), "POST", &stream_uri(&chat), json!({"content": "b"})).await;
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    // completed request id replays even though another turn is running
    let replay = env.stream(&user_a(), &chat, json!({"content": "a", "request_id": done_rid})).await;
    assert_eq!(replay.status, 200);
    assert_eq!(replay.event_names(), vec!["stream_started", "delta", "done"]);
    drop(tx);
}

#[tokio::test]
#[allow(clippy::many_single_char_names)] // reason: short throwaway response bindings in a scenario test
async fn turn_status_endpoint_maps_states() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let ok = Uuid::new_v4();
    let r = env.stream(&user_a(), &chat, json!({"content": "a", "request_id": ok})).await;
    let msg_id = r.event("stream_started").unwrap()["message_id"].clone();
    let st = env.get(&format!("/mini-chat/v1/chats/{chat}/turns/{ok}")).await;
    assert_eq!(st.status, 200, "{}", st.text());
    let v = st.json();
    assert_eq!(v["request_id"], ok.to_string());
    assert_eq!(v["state"], "done");
    assert_eq!(v["assistant_message_id"], msg_id);
    assert!(v.get("error_code").is_none());
    assert!(v["updated_at"].is_string());
    assert!(v.get("chat_id").is_none());
    for hidden in ["provider_response_id", "billing_outcome", "turn_id", "reserved_credits_micro"] {
        assert!(!st.text().contains(hidden), "turn status leaks {hidden}");
    }
    // the follow-up fetch by id from DESIGN
    let m = env
        .get(&format!("/mini-chat/v1/chats/{chat}/messages?$filter=id%20eq%20'{}'", msg_id.as_str().unwrap()))
        .await;
    assert_eq!(m.json()["items"][0]["content"], "Hello world");

    // error
    let bad = Uuid::new_v4();
    let slot = std::sync::Mutex::new(Some(json_resp(500, &json!({}))));
    env.proxy.respond(move |r| if r.uri.contains("/responses") { slot.lock().unwrap().take() } else { None });
    env.stream(&user_a(), &chat, json!({"content": "b", "request_id": bad})).await;
    let v = env.get(&format!("/mini-chat/v1/chats/{chat}/turns/{bad}")).await.json();
    assert_eq!(v["state"], "error");
    assert_eq!(v["error_code"], "provider_error");
    assert!(v.get("assistant_message_id").is_none());

    // running
    let tx = live_provider(&env.proxy);
    let run = Uuid::new_v4();
    let (_, mut body) = env.open(&user_a(), "POST", &stream_uri(&chat), json!({"content": "c", "request_id": run})).await;
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    let v = env.get(&format!("/mini-chat/v1/chats/{chat}/turns/{run}")).await.json();
    assert_eq!(v["state"], "running");
    assert!(v.get("assistant_message_id").is_none());
    assert!(v.get("error_code").is_none());
    // cancelled (client disconnect)
    drop(body);
    wait_state(&env, run, "cancelled").await;
    let v = env.get(&format!("/mini-chat/v1/chats/{chat}/turns/{run}")).await.json();
    assert_eq!(v["state"], "cancelled");
    drop(tx);

    // 404s: unknown, foreign user, malformed id
    env.get(&format!("/mini-chat/v1/chats/{chat}/turns/{}", Uuid::new_v4()))
        .await
        .assert_problem(404, "turn");
    let r = env.call(&ctx(TENANT_A, USER_A2), "GET", &format!("/mini-chat/v1/chats/{chat}/turns/{ok}"), None).await;
    assert_eq!(r.status, 404);
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/turns/not-a-uuid")).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn deleted_turn_status_is_404() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let rid = Uuid::new_v4();
    env.stream(&user_a(), &chat, json!({"content": "a", "request_id": rid})).await;
    let r = env.call(&user_a(), "DELETE", &format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), None).await;
    assert_eq!(r.status, 204);
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}")).await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn client_disconnect_cancels_with_partial_content_and_estimated_settlement() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let tx = live_provider(&env.proxy);
    let rid = Uuid::new_v4();
    let (_, mut body) = env.open(&user_a(), "POST", &stream_uri(&chat), json!({"content": "go", "request_id": rid})).await;
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    tx.send(sse_chunk("response.output_text.delta", &json!({"delta": "partial "}))).await.unwrap();
    tx.send(sse_chunk("response.output_text.delta", &json!({"delta": "answer"}))).await.unwrap();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "delta");
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "delta");
    drop(body);
    wait_state(&env, rid, "cancelled").await;
    // the provider stream is hard-cancelled
    for _ in 0..100 {
        if tx.is_closed() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(tx.is_closed(), "provider request must be aborted");
    let (_, ec, msg) = turn_row(&env, rid).await;
    assert!(ec.is_none());
    assert!(msg.is_some(), "partial content is persisted");
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    let last = msgs["items"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["content"], "partial answer");
    env.eventually("usage event", |e| e.usage_events().len() == 1).await;
    let u = &env.usage_events()[0];
    assert_eq!(u.terminal_state, "cancelled");
    assert_eq!(u.billing_outcome, "aborted");
    assert_eq!(u.settlement_method, "estimated");
    assert!(u.actual_credits_micro > 0);
    assert!(u.usage.is_none());
    let reserved = env.count("SELECT COALESCE(SUM(reserved_credits_micro), 0) FROM quota_usage").await;
    assert_eq!(reserved, 0, "reserve released at settlement");
    let spent = env.count("SELECT spent_credits_micro FROM quota_usage WHERE bucket = 'total' AND period_type = 'daily'").await;
    assert_eq!(spent, u.actual_credits_micro);
    // the chat accepts a new turn
    let r = env.send_msg(&chat, "next").await;
    assert_eq!(r.event_names().last().unwrap(), "done");
}

#[tokio::test]
async fn disconnect_before_content_cancels_without_message() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let tx = live_provider(&env.proxy);
    let rid = Uuid::new_v4();
    let (_, mut body) = env.open(&user_a(), "POST", &stream_uri(&chat), json!({"content": "go", "request_id": rid})).await;
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    drop(body);
    wait_state(&env, rid, "cancelled").await;
    let (_, _, msg) = turn_row(&env, rid).await;
    assert!(msg.is_none(), "no assistant message for empty partial content");
    let v = env.get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}")).await.json();
    assert!(v.get("assistant_message_id").is_none());
    assert_eq!(env.count("SELECT COUNT(*) FROM messages WHERE role = 'assistant'").await, 0);
    env.eventually("usage event", |e| e.usage_events().len() == 1).await;
    assert_eq!(env.usage_events()[0].billing_outcome, "aborted");
    drop(tx);
}

#[tokio::test]
async fn orphan_watchdog_finalizes_stale_turns_once() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let tx = live_provider(&env.proxy);
    let rid = Uuid::new_v4();
    let (_, mut body) = env.open(&user_a(), "POST", &stream_uri(&chat), json!({"content": "go", "request_id": rid})).await;
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    // fresh progress → not an orphan
    assert_eq!(env.svc.orphan_scan().await.unwrap(), 0);
    // simulate a crashed pod: progress stale beyond the timeout
    env.sql_exec(&format!(
        "UPDATE chat_turns SET last_progress_at = '2020-01-01T00:00:00.000001Z', started_at = '2020-01-01T00:00:00.000001Z' WHERE request_id = x'{}'",
        rid.simple()
    ))
    .await;
    assert_eq!(env.svc.orphan_scan().await.unwrap(), 1);
    let (state, ec, msg) = turn_row(&env, rid).await;
    assert_eq!((state.as_str(), ec.as_deref()), ("failed", Some("orphan_timeout")));
    assert!(msg.is_none());
    assert_eq!(env.svc.orphan_scan().await.unwrap(), 0, "CAS: finalized once");
    // the live task loses the CAS: no second settlement / usage event
    drop(body);
    drop(tx);
    env.eventually("usage event", |e| !e.usage_events().is_empty()).await;
    env.settle().await;
    let events = env.usage_events();
    assert_eq!(events.len(), 1, "exactly one usage event per turn");
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(events[0].settlement_method, "estimated");
    assert_eq!(events[0].terminal_state, "failed");
    let v = env.get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}")).await.json();
    assert_eq!(v["state"], "error");
    assert_eq!(v["error_code"], "orphan_timeout");
    assert_eq!(env.count("SELECT COALESCE(SUM(reserved_credits_micro), 0) FROM quota_usage").await, 0);
}

#[tokio::test]
async fn orphan_without_reserve_fields_is_finalized_without_settlement() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let tx = live_provider(&env.proxy);
    let rid = Uuid::new_v4();
    let (_, mut body) = env.open(&user_a(), "POST", &stream_uri(&chat), json!({"content": "go", "request_id": rid})).await;
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    env.sql_exec(&format!(
        "UPDATE chat_turns SET last_progress_at = NULL, started_at = '2020-01-01T00:00:00.000001Z', reserve_tokens = NULL WHERE request_id = x'{}'",
        rid.simple()
    ))
    .await;
    let spent_before = env.count("SELECT COALESCE(SUM(spent_credits_micro), 0) FROM quota_usage").await;
    assert_eq!(env.svc.orphan_scan().await.unwrap(), 1);
    assert_eq!(turn_row(&env, rid).await.0, "failed");
    assert_eq!(env.count("SELECT COALESCE(SUM(spent_credits_micro), 0) FROM quota_usage").await, spent_before);
    drop(body);
    drop(tx);
}
