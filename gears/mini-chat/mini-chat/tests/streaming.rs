//! US2 — send message over SSE: event contract, provider wire format, idempotency, guards,
//! provider failures, tools, cancellation, settlement and outbox (T036–T039).

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn happy_stream_event_contract_and_persistence() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    let r = env.stream("a1", chat, json!({"content": "Hi there", "request_id": rid})).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.error);
    assert_eq!(r.headers.get("content-type").unwrap(), "text/event-stream");
    let names = r.names();
    assert_eq!(names.first(), Some(&"stream_started"));
    assert_eq!(names.last(), Some(&"done"));
    assert!(names.iter().filter(|n| **n == "delta").count() >= 1);
    let started = r.event("stream_started").unwrap();
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["is_new_turn"], true);
    let msg_id = started["message_id"].as_str().unwrap().to_owned();
    assert_eq!(r.text(), "Hello from mock.");
    let done = r.event("done").unwrap();
    assert_eq!(done["usage"]["input_tokens"], 120);
    assert_eq!(done["usage"]["output_tokens"], 30);
    assert_eq!(done["effective_model"], "gpt-4.1");
    assert_eq!(done["selected_model"], "gpt-4.1");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done["quota_warnings"].is_array());
    // No provider identifiers leak into the stream.
    for (_, v) in &r.events {
        let s = v.to_string();
        assert!(!s.contains("resp_"), "provider id leaked: {s}");
    }

    // Messages persisted: user + assistant, sharing the request id.
    let (s, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    assert_eq!(s, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[1]["role"], "assistant");
    assert_eq!(items[1]["id"], msg_id.as_str());
    assert_eq!(items[1]["content"], "Hello from mock.");
    assert_eq!(items[1]["request_id"], rid.to_string());
    assert_eq!(items[1]["model"], "gpt-4.1");
    // Turn status.
    let (s, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{rid}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["state"], "done");
    assert_eq!(v["assistant_message_id"], msg_id.as_str());
    // Chat message_count.
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}"), None).await;
    assert_eq!(v["message_count"], 2);
}

#[tokio::test]
async fn provider_request_wire_format() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let r = env.stream("a1", chat, json!({"content": "Hi"})).await;
    assert_eq!(r.status, StatusCode::OK);
    let reqs = env.mock.responses_requests();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(req.path, "/v1/responses");
    // OAGW injected the credential from credstore.
    assert_eq!(req.headers.get("authorization").map(String::as_str), Some("Bearer sk-test-secret"));
    let b = &req.body;
    assert_eq!(b["model"], "gpt-4.1-provider");
    assert_eq!(b["stream"], true);
    assert_eq!(b["max_output_tokens"], 4096);
    assert!(b["instructions"].as_str().unwrap().starts_with("You are a test assistant."));
    let user = b["user"].as_str().unwrap();
    assert_eq!(user.len(), 64);
    assert_eq!(user, format!("{}{}", TENANT_A.simple(), USER_A1.simple()));
    assert_eq!(b["metadata"]["request_type"], "chat");
    assert_eq!(b["metadata"]["chat_id"], chat.to_string());
    assert_eq!(b["metadata"]["tenant_id"], TENANT_A.to_string());
    assert_eq!(b["metadata"]["feature"], "none");
    assert_eq!(b["input"].as_array().unwrap().last().unwrap()["role"], "user");
    assert!(b.get("tools").is_none_or(|t| t.as_array().is_none_or(Vec::is_empty)));
    assert_eq!(b["temperature"], 0.7);
}

#[tokio::test]
async fn history_is_sent_on_the_next_turn() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    env.stream("a1", chat, json!({"content": "first question"})).await;
    env.stream("a1", chat, json!({"content": "second question"})).await;
    let reqs = env.mock.responses_requests();
    let input = reqs[1].body["input"].as_array().unwrap().clone();
    let texts: Vec<String> = input.iter().map(ToString::to_string).collect();
    assert!(texts[0].contains("first question"));
    assert!(texts[1].contains("Hello from mock."));
    assert!(texts[2].contains("second question"));
}

#[tokio::test]
async fn replay_of_completed_request_is_side_effect_free() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    let first = env.stream("a1", chat, json!({"content": "Hi", "request_id": rid})).await;
    let calls = env.mock.responses_requests().len();
    let quota_before = env.count("SELECT COALESCE(SUM(spent_credits_micro),0) FROM quota_usage").await;
    let replay = env.stream("a1", chat, json!({"content": "Hi", "request_id": rid})).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.event("stream_started").unwrap()["is_new_turn"], false);
    assert_eq!(
        replay.event("stream_started").unwrap()["message_id"],
        first.event("stream_started").unwrap()["message_id"]
    );
    assert_eq!(replay.text(), "Hello from mock.");
    assert_eq!(replay.names().last(), Some(&"done"));
    assert_eq!(env.mock.responses_requests().len(), calls);
    assert_eq!(env.count("SELECT COALESCE(SUM(spent_credits_micro),0) FROM quota_usage").await, quota_before);
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_turns").await, 1);
}

#[tokio::test]
async fn parallel_turn_and_running_request_id_are_rejected() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    let env = std::sync::Arc::new(env);
    let e2 = env.clone();
    let slow = tokio::spawn(async move { e2.stream("a1", chat, json!({"content": "[[slow]]", "request_id": rid})).await });
    let running = env
        .eventually(Duration::from_secs(5), || async {
            (env.count("SELECT COUNT(*) FROM chat_turns WHERE state = 'running'").await == 1).then_some(())
        })
        .await;
    assert!(running.is_some());
    let r = env.stream("a1", chat, json!({"content": "other"})).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.error);
    assert_eq!(r.error["context"]["reason"], "turn_already_running");
    let r = env.stream("a1", chat, json!({"content": "[[slow]]", "request_id": rid})).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.error);
    let done = slow.await.unwrap();
    assert_eq!(done.names().last(), Some(&"done"));
}

#[tokio::test]
async fn preflight_validation_errors_open_no_stream() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let r = env.stream("a1", chat, json!({"content": "   "})).await;
    assert_problem(r.status, &r.error, 400, "invalid_argument");
    assert_eq!(violation_reason(&r.error).as_deref(), Some("EMPTY_CONTENT"));
    let r = env.stream("a1", chat, json!({"content": "hi", "attachment_ids": [Uuid::new_v4()]})).await;
    assert_problem(r.status, &r.error, 400, "invalid_argument");
    let a = Uuid::new_v4();
    let r = env.stream("a1", chat, json!({"content": "hi", "attachment_ids": [a, a]})).await;
    assert_problem(r.status, &r.error, 400, "invalid_argument");
    let r = env.stream("a1", chat, json!({"content": "x".repeat(600_000)})).await;
    assert!(r.status.is_client_error(), "{:?}", r.status);
    assert!(env.mock.responses_requests().is_empty());
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_turns").await, 0);
}

#[tokio::test]
async fn provider_failures_terminate_with_sanitized_error_events() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    let r = env.stream("a1", chat, json!({"content": "[[error]]", "request_id": rid})).await;
    assert_eq!(r.status, StatusCode::OK);
    let (name, err) = r.last();
    assert_eq!(name, "error");
    assert_eq!(err["code"], "provider_error");
    let msg = err["message"].as_str().unwrap();
    assert!(!msg.contains("file-abcdef"), "provider id leaked: {msg}");
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{rid}"), None).await;
    assert_eq!(v["state"], "error");
    assert_eq!(v["error_code"], "provider_error");

    let r = env.stream("a1", chat, json!({"content": "[[429]]"})).await;
    assert_eq!(r.last().0, "error");
    assert_eq!(r.last().1["code"], "rate_limited");
    let r = env.stream("a1", chat, json!({"content": "[[500]]"})).await;
    assert_eq!(r.last().0, "error");
    assert_eq!(r.last().1["code"], "provider_error");
    assert!(!r.last().1.to_string().contains("file-abcdef"));
}

#[tokio::test]
async fn incomplete_and_empty_responses_complete_the_turn() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let r = env.stream("a1", chat, json!({"content": "[[incomplete]]"})).await;
    assert_eq!(r.names().last(), Some(&"done"));
    let r = env.stream("a1", chat, json!({"content": "[[empty]]"})).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(matches!(r.last().0.as_str(), "done" | "error"));
}

#[tokio::test]
async fn web_search_tool_events_citations_and_limits() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let r = env.stream("a1", chat, json!({"content": "news [[web_search]]", "web_search": {"enabled": true}})).await;
    assert_eq!(r.status, StatusCode::OK);
    let tools: Vec<&Value> = r.events.iter().filter(|(n, _)| n == "tool").map(|(_, v)| v).collect();
    assert!(tools.iter().any(|t| t["name"] == "web_search" && t["phase"] == "start"));
    assert!(tools.iter().any(|t| t["name"] == "web_search" && t["phase"] == "done"));
    let cites = r.event("citations").unwrap();
    assert_eq!(cites["items"][0]["source"], "web");
    assert_eq!(cites["items"][0]["url"], "https://example.com/a");
    let req = env.mock.responses_requests().last().unwrap().body.clone();
    assert!(req["tools"].as_array().unwrap().iter().any(|t| t["type"] == "web_search"));
    assert!(req["instructions"].as_str().unwrap().contains("web_search"));
    assert_eq!(req["metadata"]["feature"], "web_search");
    // The per-turn hard limit (2) is enforced mid-turn.
    let rid = Uuid::new_v4();
    let r = env.stream("a1", chat, json!({"content": "[[web_search3]]", "web_search": {"enabled": true}, "request_id": rid})).await;
    assert_eq!(r.last().0, "error");
    assert_eq!(r.last().1["code"], "web_search_calls_exceeded");
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{rid}"), None).await;
    assert_eq!(v["error_code"], "web_search_calls_exceeded");
    // Kill switch rejects before the stream opens.
    env.policy.kill_switches(|k| k.disable_web_search = true);
    let r = env.stream("a1", chat, json!({"content": "x", "web_search": {"enabled": true}})).await;
    assert_problem(r.status, &r.error, 400, "failed_precondition");
    assert_eq!(r.error["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(r.error["context"]["violations"][0]["type"], "FEATURE_DISABLED");
}

#[tokio::test]
async fn web_search_daily_quota_is_checked_at_preflight() {
    let env = TestEnv::with(EnvOptions { config: json!({"quota": {"web_search_daily_quota": 1}}), ..EnvOptions::default() }).await;
    let chat = env.create_chat("a1", json!({})).await;
    let r = env.stream("a1", chat, json!({"content": "[[web_search]]", "web_search": {"enabled": true}})).await;
    assert_eq!(r.names().last(), Some(&"done"));
    let r = env.stream("a1", chat, json!({"content": "again", "web_search": {"enabled": true}})).await;
    assert_problem(r.status, &r.error, 429, "resource_exhausted");
    let r = env.stream("a1", chat, json!({"content": "no search"})).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn client_disconnect_cancels_the_turn_and_settles_estimated() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    let evs = env.stream_and_disconnect("a1", chat, json!({"content": "[[slow]]", "request_id": rid}), 3).await;
    assert!(evs.iter().any(|(n, _)| n == "delta"));
    let st = env
        .eventually(Duration::from_secs(10), || async {
            let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{rid}"), None).await;
            (v["state"] == "cancelled").then_some(v)
        })
        .await;
    assert!(st.is_some(), "turn not cancelled");
    let usage = env
        .eventually(Duration::from_secs(10), || async {
            env.policy.usage_events().into_iter().find(|u| u.request_id == rid)
        })
        .await
        .expect("usage event");
    assert_eq!(usage.billing_outcome, "aborted");
    assert_eq!(usage.settlement_method, "estimated");
    assert_eq!(usage.terminal_state, "cancelled");
}

#[tokio::test]
async fn usage_and_audit_events_are_published_once_per_turn() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    env.stream("a1", chat, json!({"content": "Hi", "request_id": rid})).await;
    let ev = env
        .eventually(Duration::from_secs(10), || async { env.policy.usage_events().into_iter().find(|u| u.request_id == rid) })
        .await
        .expect("usage event");
    assert_eq!(ev.billing_outcome, "completed");
    assert_eq!(ev.settlement_method, "actual");
    assert_eq!(ev.requester_type, "user");
    assert_eq!(ev.user_id, Some(USER_A1));
    // 120 in * 1.0 + 30 out * 3.0 = 210 credits_micro.
    assert_eq!(ev.actual_credits_micro, 210);
    let turn_id = ev.turn_id.unwrap();
    assert_eq!(ev.dedupe_key, format!("{}/{}/{}", TENANT_A.simple(), turn_id.simple(), rid.simple()));
    let audit = env
        .eventually(Duration::from_secs(10), || async {
            env.audit.turns.lock().unwrap().iter().find(|a| a.request_id == rid).cloned()
        })
        .await
        .expect("audit event");
    assert_eq!(audit.event_type, "turn_completed");
    assert_eq!(audit.policy_decisions.quota.decision, "allow");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.policy.usage_events().iter().filter(|u| u.request_id == rid).count(), 1);
    // quota_usage rows: total + premium buckets, daily + monthly.
    assert_eq!(env.count("SELECT COUNT(*) FROM quota_usage WHERE spent_credits_micro = 210 AND reserved_credits_micro = 0").await, 4);
}
