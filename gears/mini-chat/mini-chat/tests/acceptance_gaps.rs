//! Acceptance items not covered elsewhere: context budget (AC-02), unbuffered relay (AC-03),
//! model immutability (AC-04), activity ordering (AC-08), keepalive pings (AC-15), replay before
//! the parallel-turn guard (AC-20), concurrent mutations (AC-25), carried-forward history
//! (AC-26), null content on cancellation (AC-28), reserve-before-execute (AC-36) and per-tier
//! accounting (AC-38).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::*;
use futures::StreamExt;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn input_message_and_assembled_request_budgets() {
    let catalog = vec![
        model_entry("gpt-4.1", "Premium", json!({"max_input_tokens": 100})),
        model_entry("tiny", "Standard", json!({"context_window": 900, "max_output_tokens": 500, "max_input_tokens": 0, "preference": {"is_default": false}})),
    ];
    let env = TestEnv::with(EnvOptions { catalog, ..EnvOptions::default() }).await;
    let chat = env.create_chat("a1", json!({})).await;
    let r = env.stream("a1", chat, json!({"content": "word ".repeat(200)})).await;
    assert_problem(r.status, &r.error, 400, "out_of_range");
    assert_eq!(violation_reason(&r.error).as_deref(), Some("INPUT_TOO_LONG"));
    // A tiny context window cannot fit system prompt + message + output reserve.
    let chat = env.create_chat("a1", json!({"model": "tiny"})).await;
    let r = env.stream("a1", chat, json!({"content": "word ".repeat(400)})).await;
    assert!(r.status.is_client_error(), "{:?} {}", r.status, r.error);
    assert!(env.mock.responses_requests().is_empty());
}

#[tokio::test]
async fn deltas_are_relayed_before_the_provider_finishes() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let started = Instant::now();
    let r = env.call("a1", Method::POST, &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "[[slow]]"}))).await;
    let mut s = r.into_body().into_data_stream();
    let mut buf = String::new();
    let mut first_delta = None;
    while let Some(Ok(chunk)) = s.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if first_delta.is_none() && buf.contains("event: delta") {
            first_delta = Some(started.elapsed());
        }
    }
    let total = started.elapsed();
    let first = first_delta.expect("delta");
    // The mock spreads 10 deltas over ~3 s; the first one must arrive long before the end.
    assert!(total >= Duration::from_millis(2500), "total {total:?}");
    assert!(first < total / 2, "first delta at {first:?}, total {total:?}");
}

#[tokio::test]
async fn chat_model_is_immutable() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({"model": "gpt-4.1"})).await;
    let _ = env.json("a1", Method::PATCH, &format!("/chats/{chat}"), Some(json!({"title": "t", "model": "gpt-4.1-mini"}))).await;
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}"), None).await;
    assert_eq!(v["model"], "gpt-4.1");
    // A downgraded turn does not change the chat model either.
    env.policy.kill_switches(|k| k.force_standard_tier = true);
    env.stream("a1", chat, json!({"content": "hi"})).await;
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}"), None).await;
    assert_eq!(v["model"], "gpt-4.1");
}

#[tokio::test]
async fn chat_list_reflects_most_recent_activity() {
    let env = TestEnv::start().await;
    let older = env.create_chat("a1", json!({"title": "older"})).await;
    let newer = env.create_chat("a1", json!({"title": "newer"})).await;
    let (_, v) = env.json("a1", Method::GET, "/chats", None).await;
    assert_eq!(v["items"][0]["id"], newer.to_string());
    env.stream("a1", older, json!({"content": "bump"})).await;
    let (_, v) = env.json("a1", Method::GET, "/chats", None).await;
    assert_eq!(v["items"][0]["id"], older.to_string());
    env.json("a1", Method::PATCH, &format!("/chats/{newer}"), Some(json!({"title": "renamed"}))).await;
    let (_, v) = env.json("a1", Method::GET, "/chats", None).await;
    assert_eq!(v["items"][0]["id"], newer.to_string());
}

#[tokio::test]
async fn ping_events_are_sent_until_the_first_content() {
    let env = TestEnv::with(EnvOptions { config: json!({"streaming": {"sse_ping_interval_seconds": 5}}), ..EnvOptions::default() }).await;
    // The first delta arrives after 5.6 s, beyond the 5 s ping interval.
    env.mock.configure(|c| c.slow_delay = Duration::from_millis(5600));
    let chat = env.create_chat("a1", json!({})).await;
    let r = env.call("a1", Method::POST, &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "[[slow]]"}))).await;
    let mut s = r.into_body().into_data_stream();
    let mut buf = String::new();
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        let Ok(Some(Ok(chunk))) = tokio::time::timeout(Duration::from_secs(12), s.next()).await else { break };
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if buf.matches("event: delta").count() >= 1 {
            break;
        }
    }
    drop(s);
    let events = parse_sse(&buf);
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names[0], "stream_started");
    let ping_pos = names.iter().position(|n| *n == "ping").expect("ping before first content");
    let delta_pos = names.iter().position(|n| *n == "delta").expect("delta");
    assert!(ping_pos < delta_pos, "{names:?}");
}

#[tokio::test]
async fn replay_is_checked_before_the_parallel_turn_guard() {
    let env = Arc::new(TestEnv::start().await);
    let chat = env.create_chat("a1", json!({})).await;
    let done_rid = Uuid::new_v4();
    env.stream("a1", chat, json!({"content": "first", "request_id": done_rid})).await;
    let e2 = env.clone();
    let slow = tokio::spawn(async move { e2.stream("a1", chat, json!({"content": "[[slow]]"})).await });
    env.eventually(Duration::from_secs(5), || async {
        (env.count("SELECT COUNT(*) FROM chat_turns WHERE state = 'running'").await == 1).then_some(())
    })
    .await
    .unwrap();
    let replay = env.stream("a1", chat, json!({"content": "first", "request_id": done_rid})).await;
    assert_eq!(replay.status, StatusCode::OK, "{:?}", replay.error);
    assert_eq!(replay.event("stream_started").unwrap()["is_new_turn"], false);
    slow.await.unwrap();
}

#[tokio::test]
async fn concurrent_mutations_resolve_to_a_single_winner() {
    let env = Arc::new(TestEnv::start().await);
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    env.stream("a1", chat, json!({"content": "q", "request_id": rid})).await;
    let mut handles = Vec::new();
    for _ in 0..3 {
        let e = env.clone();
        handles.push(tokio::spawn(async move {
            e.stream_path("a1", &format!("/chats/{chat}/turns/{rid}/retry"), Method::POST, json!({})).await.status
        }));
    }
    let mut statuses = Vec::new();
    for h in handles {
        statuses.push(h.await.unwrap());
    }
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::OK).count(), 1, "{statuses:?}");
    assert!(statuses.iter().all(|s| *s == StatusCode::OK || *s == StatusCode::CONFLICT), "{statuses:?}");
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_turns WHERE deleted_at IS NULL").await, 1);
}

#[tokio::test]
async fn retry_carries_forward_attachments_and_web_search() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let (ct, body) = multipart("p.png", Some("image/png"), &png(16, 16));
    let r = env
        .raw("a1", Request::builder().method(Method::POST).uri(format!("/mini-chat/v1/chats/{chat}/attachments")).header("content-type", ct), Body::from(body))
        .await;
    let img: Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
    let rid = Uuid::new_v4();
    env.stream("a1", chat, json!({"content": "look", "request_id": rid, "attachment_ids": [img["id"]], "web_search": {"enabled": true}})).await;
    let r = env.stream_path("a1", &format!("/chats/{chat}/turns/{rid}/retry"), Method::POST, json!({})).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.error);
    let req = env.mock.responses_requests().last().unwrap().body.clone();
    assert!(req["tools"].as_array().unwrap().iter().any(|t| t["type"] == "web_search"));
    let last = req["input"].as_array().unwrap().last().unwrap().clone();
    assert!(last["content"].as_array().unwrap().iter().any(|p| p["type"] == "input_image"));
    let (_, msgs) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    assert_eq!(msgs["items"][0]["attachments"][0]["attachment_id"], img["id"]);
}

#[tokio::test]
async fn cancellation_before_any_content_persists_no_assistant_message() {
    let env = TestEnv::start().await;
    env.mock.configure(|c| c.slow_delay = Duration::from_secs(3));
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    let evs = env.stream_and_disconnect("a1", chat, json!({"content": "[[slow]]", "request_id": rid}), 1).await;
    assert_eq!(evs[0].0, "stream_started");
    env.eventually(Duration::from_secs(10), || async {
        let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{rid}"), None).await;
        (v["state"] == "cancelled").then_some(())
    })
    .await
    .expect("cancelled");
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "only the user message: {v}");
    assert_eq!(items[0]["role"], "user");
}

#[tokio::test]
async fn reserve_is_taken_before_the_provider_call_and_released_after() {
    let env = Arc::new(TestEnv::start().await);
    let chat = env.create_chat("a1", json!({})).await;
    let e2 = env.clone();
    let h = tokio::spawn(async move { e2.stream("a1", chat, json!({"content": "[[slow]]"})).await });
    let reserved = env
        .eventually(Duration::from_secs(5), || async {
            let n = env.count("SELECT COALESCE(SUM(reserved_credits_micro),0) FROM quota_usage").await;
            (n > 0).then_some(n)
        })
        .await;
    assert!(reserved.is_some(), "no reserve while the turn runs");
    h.await.unwrap();
    assert_eq!(env.count("SELECT COALESCE(SUM(reserved_credits_micro),0) FROM quota_usage").await, 0);
}

#[tokio::test]
async fn downgraded_turns_are_charged_to_the_standard_tier_only() {
    let env = TestEnv::start().await;
    env.policy.kill_switches(|k| k.force_standard_tier = true);
    let chat = env.create_chat("a1", json!({})).await;
    env.stream("a1", chat, json!({"content": "hi"})).await;
    assert_eq!(env.count("SELECT COALESCE(SUM(spent_credits_micro),0) FROM quota_usage WHERE bucket = 'tier:premium'").await, 0);
    assert_eq!(env.count("SELECT COUNT(*) FROM quota_usage WHERE bucket = 'total' AND spent_credits_micro = 210").await, 2);
    // Turn counters: one call per period on the total bucket.
    assert_eq!(env.count("SELECT COALESCE(SUM(calls),0) FROM quota_usage WHERE bucket = 'total'").await, 2);
}
