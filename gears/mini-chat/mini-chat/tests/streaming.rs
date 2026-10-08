//! Send-message streaming: SSE contract, preflight, persistence, replay,
//! parallel-turn guard, quota enforcement/downgrade, web search, context
//! assembly and error sanitization.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use chrono::{Datelike, Utc};
use common::*;
use mini_chat::domain::service::stream::{SendRequest, StreamEvent, StreamStart};
use serde_json::{Value, json};
use uuid::Uuid;

fn today() -> String {
    Utc::now().date_naive().to_string()
}

fn month_start() -> String {
    let d = Utc::now().date_naive();
    d.with_day(1).unwrap().to_string()
}

/// Seeds a quota_usage row (UUIDs as 16-byte blobs, like the gear writes them).
async fn seed_usage(h: &Harness, c: &toolkit_security::SecurityContext, period: &str, start: &str, bucket: &str, spent: i64) {
    h.exec(&format!(
        "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, spent_credits_micro, \
         reserved_credits_micro, calls, input_tokens, output_tokens, file_search_calls, web_search_calls, \
         code_interpreter_calls, rag_retrieval_calls, image_inputs, image_upload_bytes, updated_at) VALUES \
         (X'{}', X'{}', X'{}', '{period}', '{start}', '{bucket}', {spent}, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, '2026-01-01 00:00:00+00:00') \
         ON CONFLICT (tenant_id, user_id, period_type, period_start, bucket) DO UPDATE SET spent_credits_micro = excluded.spent_credits_micro",
        hex_uuid(Uuid::new_v4()),
        hex_uuid(c.subject_tenant_id()),
        hex_uuid(c.subject_id())
    ))
    .await;
}

fn done(evs: &[(String, Value)]) -> Value {
    evs.iter().find(|(n, _)| n == "done").map(|(_, d)| d.clone()).expect("done event")
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_event_order_request_id_and_persistence() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let rid = Uuid::new_v4();
    let r = h.send(&u, chat, json!({"content": "Hi there", "request_id": rid})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert!(r.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let evs = r.sse();
    let names: Vec<&str> = evs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["stream_started", "delta", "delta", "done"]);
    let started = &evs[0].1;
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["is_new_turn"], true);
    assert!(started.get("thread_summary_applied").is_none());
    assert_eq!(evs[1].1, json!({"type": "text", "content": "Hello"}));
    let d = done(&evs);
    assert_eq!(d["usage"], json!({"input_tokens": 100, "output_tokens": 50}));
    assert_eq!((d["effective_model"].as_str(), d["selected_model"].as_str()), (Some("std"), Some("std")));
    assert_eq!(d["quota_decision"], "allow");
    assert!(d.get("downgrade_from").is_none() && d.get("request_id").is_none());
    assert!(d["quota_warnings"].as_array().is_some());
    assert!(!r.text.contains("resp_abc123"), "provider ids never leak");

    // messages persisted with correlation and usage
    let msgs = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}/messages"), None).await.json();
    let items = msgs["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[1]["role"], "assistant");
    assert_eq!(items[1]["id"], started["message_id"]);
    assert_eq!(items[1]["content"], "Hello world");
    assert_eq!(items[1]["model"], "std");
    assert_eq!(items[1]["input_tokens"], 100);
    assert!(items.iter().all(|m| m["request_id"] == rid.to_string()));
    let turn = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), None).await.json();
    assert_eq!(turn["state"], "done");
    assert_eq!(turn["assistant_message_id"], started["message_id"]);
    // DB: turn row with preflight fields and settled quota
    let rid_hex = hex_uuid(rid);
    assert_eq!(
        h.scalar_str(&format!("SELECT state FROM chat_turns WHERE hex(request_id)='{rid_hex}'")).await.as_deref(),
        Some("completed")
    );
    assert!(h.scalar_i64(&format!("SELECT reserve_tokens FROM chat_turns WHERE hex(request_id)='{rid_hex}'")).await > 0);
    assert_eq!(
        h.scalar_i64("SELECT reserved_credits_micro FROM quota_usage WHERE bucket='total' AND period_type='daily'").await,
        0
    );
    assert_eq!(
        h.scalar_i64("SELECT spent_credits_micro FROM quota_usage WHERE bucket='total' AND period_type='daily'").await,
        250
    );
    // provider request shape
    let req = &h.provider.chat_requests()[0];
    assert_eq!(req["model"], "std-provider");
    assert_eq!(req["stream"], true);
    assert_eq!(req["instructions"], "You are a test assistant.");
    assert_eq!(req["max_output_tokens"], 1000);
    assert_eq!(req["user"].as_str().unwrap().len(), 64);
    assert_eq!(req["metadata"]["request_type"], "chat");
    assert_eq!(req["metadata"]["chat_id"], chat.to_string());
    assert_eq!(req["input"].as_array().unwrap().last().unwrap()["content"], "Hi there");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_generates_request_id_when_omitted() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    let evs = h.send(&u, chat, json!({"content": "x"})).await.sse();
    let rid = Uuid::parse_str(evs[0].1["request_id"].as_str().unwrap()).unwrap();
    assert_eq!(rid.get_version_num(), 4);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn preflight_rejections_are_json_and_have_no_side_effects() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let cases = vec![
        (json!({"content": "   "}), 400, "EMPTY_CONTENT"),
        (json!({"content": "x", "attachment_ids": [Uuid::new_v4()]}), 400, "invalid_attachment"),
        (json!({"content": "x", "attachment_ids": [Uuid::nil(), Uuid::nil()]}), 400, "invalid_attachment"),
    ];
    for (body, status, why) in cases {
        let r = h.send(&u, chat, body.clone()).await;
        assert_eq!(r.status, status, "{body}: {}", r.text);
        assert!(r.headers["content-type"].to_str().unwrap().contains("json"));
        assert_eq!(reason(&r.json()), why);
    }
    let r = h.send(&u, chat, json!({"content": "x", "attachment_ids": ["not-a-uuid"]})).await;
    assert_eq!(r.status, 422);
    let r = h.send(&u, chat, json!({})).await;
    assert_eq!(r.status, 422);
    let r = h.send(&u, Uuid::new_v4(), json!({"content": "x"})).await;
    assert_eq!(r.status, 404);
    assert!(h.provider.chat_requests().is_empty(), "no provider call on preflight failure");
    assert_eq!(h.scalar_i64("SELECT count(*) FROM chat_turns").await, 0);
    assert_eq!(h.scalar_i64("SELECT count(*) FROM messages").await, 0);
    assert_eq!(h.scalar_i64("SELECT coalesce(sum(reserved_credits_micro),0) FROM quota_usage").await, 0);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_is_side_effect_free_and_conflicts_are_rejected() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let rid = Uuid::new_v4();
    let first = h.send(&u, chat, json!({"content": "q", "request_id": rid})).await.sse();
    h.eventually(|| async { h.policy.published.lock().len() == 1 }).await;
    let spent_before = h.scalar_i64("SELECT coalesce(sum(spent_credits_micro),0) FROM quota_usage").await;
    let calls_before = h.provider.chat_requests().len();
    let outbox_before = h.scalar_i64("SELECT count(*) FROM toolkit_outbox_body").await;

    let replay = h.send(&u, chat, json!({"content": "q", "request_id": rid})).await;
    assert_eq!(replay.status, 200);
    let evs = replay.sse();
    let names: Vec<&str> = evs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["stream_started", "delta", "done"]);
    assert_eq!(evs[0].1["is_new_turn"], false);
    assert_eq!(evs[0].1["message_id"], first[0].1["message_id"]);
    assert_eq!(evs[1].1["content"], "Hello world");
    assert_eq!(done(&evs)["usage"], json!({"input_tokens": 100, "output_tokens": 50}));
    assert!(done(&evs).get("quota_warnings").is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.provider.chat_requests().len(), calls_before, "no provider call on replay");
    assert_eq!(h.scalar_i64("SELECT coalesce(sum(spent_credits_micro),0) FROM quota_usage").await, spent_before);
    assert!(h.scalar_i64("SELECT count(*) FROM toolkit_outbox_body").await <= outbox_before);
    assert_eq!(h.policy.published.lock().len(), 1, "no new usage event");

    // failed turn's request_id → conflict
    h.provider.push(Reply::Events(vec![ev(
        "response.failed",
        json!({"type": "response.failed", "response": {"error": {"message": "boom"}}}),
    )]));
    let frid = Uuid::new_v4();
    let f = h.send(&u, chat, json!({"content": "q2", "request_id": frid})).await;
    assert_eq!(f.sse().last().unwrap().0, "error");
    let again = h.send(&u, chat, json!({"content": "q2", "request_id": frid})).await;
    assert_eq!(again.status, 409);
    assert_eq!(again.json()["context"]["reason"], "request_id_conflict");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_turn_guard_and_replay_priority() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let done_rid = Uuid::new_v4();
    assert_eq!(h.send(&u, chat, json!({"content": "a", "request_id": done_rid})).await.status, 200);

    h.provider.push(Reply::Hang(vec![delta("partial")]));
    let running_rid = Uuid::new_v4();
    let start = h
        .svc
        .send_message(&u, chat, SendRequest {
            content: "b".into(),
            request_id: Some(running_rid),
            attachment_ids: vec![],
            web_search: false,
        })
        .await
        .unwrap();
    let StreamStart::Live { mut events, cancel } = start else { panic!("expected live") };
    let first = next_n(&mut events, 2).await;
    assert!(matches!(first[0], StreamEvent::Started { .. }));

    // another request while running → 409 turn_already_running
    let r = h.send(&u, chat, json!({"content": "c"})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["context"]["reason"], "turn_already_running");
    // same request_id as the running one → request_id_conflict
    let r = h.send(&u, chat, json!({"content": "b", "request_id": running_rid})).await;
    assert_eq!(r.json()["context"]["reason"], "request_id_conflict");
    // replay of a completed turn wins over the parallel guard
    let r = h.send(&u, chat, json!({"content": "a", "request_id": done_rid})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.sse()[0].1["is_new_turn"], false);
    let st = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}/turns/{running_rid}"), None).await.json();
    assert_eq!(st["state"], "running");
    assert!(st.get("assistant_message_id").is_none());

    // disconnect → cancelled, then a new turn is accepted
    drop(events);
    drop(cancel);
    wait_terminal(&h, running_rid).await;
    let st = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}/turns/{running_rid}"), None).await.json();
    assert_eq!(st["state"], "cancelled");
    assert!(st.get("assistant_message_id").is_some(), "partial content persisted");
    let r = h.send(&u, chat, json!({"content": "d"})).await;
    assert_eq!(r.status, 200);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_only_before_first_content() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    *h.provider.delay_ms.lock() = 5_600;
    h.provider.push(Reply::Events(vec![delta("late"), completed(1, 1)]));
    let start = h
        .svc
        .send_message(&u, chat, SendRequest { content: "x".into(), request_id: None, attachment_ids: vec![], web_search: false })
        .await
        .unwrap();
    let evs = collect(start).await;
    let n = names(&evs);
    assert_eq!(n[0], "stream_started");
    assert_eq!(n[1], "ping", "{n:?}");
    let first_content = n.iter().position(|e| *e == "delta").unwrap();
    assert!(n[first_content..].iter().all(|e| *e != "ping"));
    assert_eq!(*n.last().unwrap(), "done");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_errors_are_sanitized_terminal_errors() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    h.provider.push(Reply::Events(vec![
        delta("part"),
        ev(
            "response.failed",
            json!({"type": "response.failed", "response": {"error": {"message": "failed resp_abcdef123 file-ABCDEFGHIJKLMN key sk-abcdefghijklmn at https://x.example/v1"}}}),
        ),
    ]));
    let r = h.send(&u, chat, json!({"content": "x"})).await;
    let evs = r.sse();
    let (name, data) = evs.last().unwrap();
    assert_eq!(name, "error");
    assert_eq!(data["code"], "provider_error");
    let msg = data["message"].as_str().unwrap();
    for leak in ["resp_abcdef123", "file-ABCDEFGHIJKLMN", "sk-abcdefghijklmn", "https://"] {
        assert!(!msg.contains(leak), "{msg}");
    }
    assert!(msg.contains("[provider_id]") && msg.contains("[credential]") && msg.contains("[url]"));
    let rid = evs[0].1["request_id"].as_str().unwrap().to_owned();
    let st = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), None).await.json();
    assert_eq!((st["state"].as_str(), st["error_code"].as_str()), (Some("error"), Some("provider_error")));
    assert!(st.get("assistant_message_id").is_none());

    h.provider.push(Reply::Status(429, json!({"error": {"message": "slow down"}}), Some(7)));
    let r = h.send(&u, chat, json!({"content": "y"})).await.sse();
    let (name, data) = r.last().unwrap();
    assert_eq!((name.as_str(), data["code"].as_str()), ("error", Some("rate_limited")));
    assert!(data["message"].as_str().unwrap().contains('7'));

    h.provider.push(Reply::Status(500, json!({"error": {"message": "internal vs_abcdefghijklmnop"}}), None));
    let r = h.send(&u, chat, json!({"content": "z"})).await.sse();
    let (_, data) = r.last().unwrap();
    assert_eq!(data["code"], "provider_error");
    assert!(!data["message"].as_str().unwrap().contains("vs_abcdefghijklmnop"));
    // failed turns settle once with an estimated charge
    h.eventually(|| async { h.policy.published.lock().len() == 3 }).await;
    let published = h.policy.published.lock().clone();
    assert!(published.iter().all(|e| e.billing_outcome == "failed" && e.settlement_method == "estimated"));
    assert_eq!(h.scalar_i64("SELECT reserved_credits_micro FROM quota_usage WHERE bucket='total' AND period_type='daily'").await, 0);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_downgrade_and_rejection() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "prem"})).await;
    seed_usage(&h, &u, "daily", &today(), "tier:premium", 50_000_000).await;
    let r = h.send(&u, chat, json!({"content": "hello"})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let d = done(&r.sse());
    assert_eq!(d["quota_decision"], "downgrade");
    assert_eq!(d["selected_model"], "prem");
    assert_eq!(d["effective_model"], "std");
    assert_eq!(d["downgrade_from"], "prem");
    assert_eq!(d["downgrade_reason"], "premium_quota_exhausted");
    assert_eq!(h.provider.chat_requests()[0]["model"], "std-provider");
    let msgs = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}/messages"), None).await.json();
    assert_eq!(msgs["items"][1]["model"], "std");
    assert_eq!(
        h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}"), None).await.json()["model"],
        "prem",
        "selected model never changes"
    );
    // standard turn charges only the total bucket
    assert_eq!(
        h.scalar_i64("SELECT spent_credits_micro FROM quota_usage WHERE bucket='tier:premium' AND period_type='daily'").await,
        50_000_000
    );

    // all tiers exhausted (monthly total) → 429 before any provider call
    seed_usage(&h, &u, "monthly", &month_start(), "total", 1_000_000_000).await;
    let calls = h.provider.chat_requests().len();
    let r = h.send(&u, chat, json!({"content": "again"})).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(r.json()["context"]["violations"][0]["description"], "quota_exceeded");
    assert_eq!(h.provider.chat_requests().len(), calls);
    let q = h.call(&u, "GET", "/mini-chat/v1/quota/status", None).await.json();
    let monthly_total = &q["tiers"][1]["periods"][1];
    assert_eq!(monthly_total["exhausted"], true);
    assert_eq!(monthly_total["warning"], true);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn premium_turn_reserves_and_settles_both_buckets() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "prem"})).await;
    h.provider.push(Reply::Hang(vec![delta("x")]));
    let start = h
        .svc
        .send_message(&u, chat, SendRequest { content: "x".into(), request_id: None, attachment_ids: vec![], web_search: false })
        .await
        .unwrap();
    let StreamStart::Live { mut events, cancel } = start else { panic!() };
    next_n(&mut events, 2).await;
    let reserved_total = h.scalar_i64("SELECT reserved_credits_micro FROM quota_usage WHERE bucket='total' AND period_type='daily'").await;
    let reserved_prem = h.scalar_i64("SELECT reserved_credits_micro FROM quota_usage WHERE bucket='tier:premium' AND period_type='daily'").await;
    assert!(reserved_total > 0);
    assert_eq!(reserved_total, reserved_prem);
    let turn_reserved = h.scalar_i64("SELECT reserved_credits_micro FROM chat_turns").await;
    assert_eq!(turn_reserved, reserved_total);
    drop(events);
    drop(cancel);
    h.eventually(|| async { h.scalar_str("SELECT state FROM chat_turns").await.as_deref() == Some("cancelled") }).await;
    for b in ["total", "tier:premium"] {
        assert_eq!(h.scalar_i64(&format!("SELECT reserved_credits_micro FROM quota_usage WHERE bucket='{b}' AND period_type='daily'")).await, 0);
        assert!(h.scalar_i64(&format!("SELECT spent_credits_micro FROM quota_usage WHERE bucket='{b}' AND period_type='daily'")).await > 0);
        assert_eq!(h.scalar_i64(&format!("SELECT calls FROM quota_usage WHERE bucket='{b}' AND period_type='daily'")).await, 1);
    }
    h.eventually(|| async { h.policy.published.lock().len() == 1 }).await;
    let e = h.policy.published.lock()[0].clone();
    assert_eq!((e.billing_outcome.as_str(), e.settlement_method.as_str(), e.terminal_state.as_str()), ("aborted", "estimated", "cancelled"));
    assert!(e.usage.is_none());
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn web_search_tool_citations_limits_and_quota() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    h.provider.push(Reply::Events(vec![
        ev("response.web_search_call.searching", json!({"type": "response.web_search_call.searching"})),
        ev("response.web_search_call.completed", json!({"type": "response.web_search_call.completed"})),
        delta("Answer"),
        ev(
            "response.output_text.annotation.added",
            json!({"type": "response.output_text.annotation.added", "annotation": {"type": "url_citation", "url": "https://news.example/a", "title": "News", "start_index": 0, "end_index": 6}}),
        ),
        completed(10, 5),
    ]));
    let r = h.send(&u, chat, json!({"content": "news?", "web_search": {"enabled": true}})).await;
    let evs = r.sse();
    let names: Vec<&str> = evs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["stream_started", "tool", "tool", "delta", "citations", "done"]);
    assert_eq!(evs[1].1, json!({"phase": "start", "name": "web_search", "details": {}}));
    assert_eq!(evs[2].1["phase"], "done");
    let cit = &evs[4].1["items"][0];
    assert_eq!(cit["source"], "web");
    assert_eq!(cit["url"], "https://news.example/a");
    assert_eq!(cit["snippet"], "Answer");
    assert_eq!(cit["span"], json!({"start": 0, "end": 6}));
    let req = &h.provider.chat_requests()[0];
    assert!(req["tools"].as_array().unwrap().iter().any(|t| t["type"] == "web_search"));
    assert!(req["instructions"].as_str().unwrap().contains("Use web_search only"));
    assert_eq!(req["metadata"]["feature"], "web_search");
    assert_eq!(h.scalar_i64("SELECT web_search_completed_count FROM chat_turns").await, 1);
    assert_eq!(h.scalar_i64("SELECT web_search_calls FROM quota_usage WHERE bucket='total' AND period_type='daily'").await, 1);

    // per-message limit (default 2): third start fails the turn
    let mut frames = Vec::new();
    for _ in 0..3 {
        frames.push(ev("response.web_search_call.searching", json!({"type": "response.web_search_call.searching"})));
    }
    frames.push(completed(1, 1));
    h.provider.push(Reply::Events(frames));
    let r = h.send(&u, chat, json!({"content": "more", "web_search": {"enabled": true}})).await.sse();
    let (name, data) = r.last().unwrap();
    assert_eq!((name.as_str(), data["code"].as_str()), ("error", Some("web_search_calls_exceeded")));

    // daily quota exhausted → 429 web_search, only for requests that enable it
    h.exec(&format!(
        "UPDATE quota_usage SET web_search_calls = 75 WHERE bucket='total' AND period_type='daily' AND hex(user_id)='{}'",
        hex_uuid(u.subject_id())
    ))
    .await;
    let r = h.send(&u, chat, json!({"content": "again", "web_search": {"enabled": true}})).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(h.send(&u, chat, json!({"content": "no tools"})).await.status, 200);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_switches_and_vision_guards() {
    let h = Harness::new().await;
    let u = user();
    let mut pc = policy_cfg(default_catalog());
    pc.kill_switches.disable_web_search = true;
    h.set_policy(&pc);
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let r = h.send(&u, chat, json!({"content": "x", "web_search": {"enabled": true}})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(r.json()["context"]["violations"][0]["type"], "FEATURE_DISABLED");

    // image on a model without VISION_INPUT → 400 VISION_NOT_SUPPORTED, no provider call
    h.set_policy(&policy_cfg(default_catalog()));
    let nv = h.create_chat(&u, json!({"model": "novision"})).await;
    let up = h.upload(&u, nv, "a.png", "image/png", &png(20, 20)).await;
    assert_eq!(up.status, 201, "{}", up.text);
    let aid = up.json()["id"].as_str().unwrap().to_owned();
    let calls = h.provider.chat_requests().len();
    let r = h.send(&u, nv, json!({"content": "what", "attachment_ids": [aid]})).await;
    assert_eq!(r.status, 400);
    assert_eq!(reason(&r.json()), "VISION_NOT_SUPPORTED");
    assert_eq!(h.provider.chat_requests().len(), calls);
    // disable_images rejects the request
    let mut pc = policy_cfg(default_catalog());
    pc.kill_switches.disable_images = true;
    h.set_policy(&pc);
    let r = h.send(&u, nv, json!({"content": "what", "attachment_ids": [aid]})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "images");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn context_assembly_history_and_budget() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    h.send(&u, chat, json!({"content": "first question"})).await;
    h.send(&u, chat, json!({"content": "second question"})).await;
    let reqs = h.provider.chat_requests();
    let input = reqs[1]["input"].as_array().unwrap();
    let contents: Vec<&str> = input.iter().map(|m| m["content"].as_str().unwrap()).collect();
    assert_eq!(contents, vec!["first question", "Hello world", "second question"]);
    let roles: Vec<&str> = input.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user"]);
    assert!(reqs[1].get("tools").is_none(), "no tools without attachments / web search");

    // tiny model: message over max_input_tokens → INPUT_TOO_LONG; huge mandatory context → budget error
    let tiny = h.create_chat(&u, json!({"model": "tiny"})).await;
    let r = h.send(&u, tiny, json!({"content": "x".repeat(4000)})).await;
    assert_eq!(r.status, 400);
    assert_eq!(reason(&r.json()), "INPUT_TOO_LONG");
    let mut catalog = default_catalog();
    let mut nolimit = model("nolimit", "standard", false, true);
    nolimit["context_window"] = json!(1000);
    nolimit["max_output_tokens"] = json!(200);
    catalog.push(nolimit);
    h.set_policy(&policy_cfg(catalog));
    let nl = h.create_chat(&u, json!({"model": "nolimit"})).await;
    let r = h.send(&u, nl, json!({"content": "x".repeat(2800)})).await;
    assert_eq!(r.status, 400, "{}", r.text);
    assert_eq!(reason(&r.json()), "CONTEXT_BUDGET_EXCEEDED");
    // history is truncated deterministically (oldest first) on the tiny model
    for i in 0..4 {
        let r = h.send(&u, tiny, json!({"content": format!("{i} {}", "y".repeat(600))})).await;
        assert_eq!(r.status, 200, "{}", r.text);
    }
    let last = h.provider.chat_requests().last().unwrap().clone();
    let input = last["input"].as_array().unwrap();
    assert!(input.len() < 8, "older turns dropped");
    assert_eq!(input[0]["role"], "user", "never starts with an orphan answer");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_published_once_with_canonical_dedupe_key() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let rid = Uuid::new_v4();
    h.send(&u, chat, json!({"content": "x", "request_id": rid})).await;
    h.eventually(|| async { h.policy.published.lock().len() == 1 }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let events = h.policy.published.lock().clone();
    assert_eq!(events.len(), 1);
    let e = &events[0];
    let turn_id = Uuid::parse_str(&h.scalar_str("SELECT lower(hex(id)) FROM chat_turns").await.unwrap()).unwrap();
    assert_eq!(
        e.dedupe_key,
        format!("{}/{}/{}", u.subject_tenant_id().as_simple(), turn_id.as_simple(), rid.as_simple())
    );
    assert_eq!(e.turn_id, Some(turn_id));
    assert_eq!((e.billing_outcome.as_str(), e.settlement_method.as_str()), ("completed", "actual"));
    assert_eq!(e.actual_credits_micro, 250);
    assert_eq!(e.policy_version_applied, 1);
    assert_eq!(e.requester_type, "user");
    assert_eq!(e.usage.unwrap().input_tokens, 100);
    h.eventually(|| async { h.audit.turns.lock().len() == 1 }).await;
    let a = h.audit.turns.lock()[0].clone();
    assert_eq!(a.event_type, "turn_completed");
    assert_eq!(a.policy_decisions.quota.decision, "allow");
    assert!(a.prompt.is_empty() && a.response.is_empty());
    h.shutdown().await;
}
