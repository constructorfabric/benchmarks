//! Router tests: reserve → execute → settle, tier downgrade, kill switches,
//! 429 quota scopes, usage events, web search and the quota status API
//! (acceptance: Quota Enforcement, Settlement & Finalization, Quota Status API,
//! Web Search, Principles "quota checked before any provider call").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::infra::llm::sse_parser::SseParser;
use crate::test_support::{
    EnvOptions, TestEnv, default_catalog, live_provider, next_event, sse_chunk, sse_resp, user_a,
};

async fn bucket(env: &TestEnv, bucket: &str, period: &str) -> (i64, i64, i64) {
    let rows = env
        .sql_rows(&format!(
            "SELECT spent_credits_micro, reserved_credits_micro, calls FROM quota_usage WHERE bucket = '{bucket}' AND period_type = '{period}'"
        ))
        .await;
    rows.first().map_or((0, 0, 0), |r| {
        (
            r.try_get_by_index::<i64>(0).unwrap(),
            r.try_get_by_index::<i64>(1).unwrap(),
            r.try_get_by_index::<i64>(2).unwrap(),
        )
    })
}

fn env_with_policy(policy_extra: Value, config: Value) -> EnvOptions {
    let mut policy = json!({"model_catalog": default_catalog()});
    if let (Value::Object(p), Value::Object(e)) = (&mut policy, policy_extra) {
        p.extend(e);
    }
    EnvOptions { config, policy }
}

#[tokio::test]
async fn reserve_during_stream_then_actual_settlement() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let tx = live_provider(&env.proxy);
    let rid = Uuid::new_v4();
    let (_, mut body) = env
        .open(
            &user_a(),
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            json!({"content": "hello", "request_id": rid}),
        )
        .await;
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    let reserved_turn = env
        .count(&format!("SELECT reserved_credits_micro FROM chat_turns WHERE request_id = x'{}'", rid.simple()))
        .await;
    assert!(reserved_turn > 0);
    for b in ["total", "tier:premium"] {
        for period in ["daily", "monthly"] {
            let (spent, reserved, _) = bucket(&env, b, period).await;
            assert_eq!((spent, reserved), (0, reserved_turn), "{b}/{period} holds the reserve while running");
        }
    }
    tx.send(sse_chunk("response.output_text.delta", &json!({"delta": "hi"}))).await.unwrap();
    tx.send(sse_chunk(
        "response.completed",
        &json!({"response": {"usage": {"input_tokens": 1000, "output_tokens": 333}}}),
    ))
    .await
    .unwrap();
    loop {
        if next_event(&mut body, &mut p, &mut q).await.unwrap().0 == "done" {
            break;
        }
    }
    // gpt-4.1: 3 / 9 credits per token (micro multipliers 3_000_000 / 9_000_000)
    let expected = 1000 * 3 + 333 * 9;
    for b in ["total", "tier:premium"] {
        for period in ["daily", "monthly"] {
            let (spent, reserved, calls) = bucket(&env, b, period).await;
            assert_eq!((spent, reserved, calls), (expected, 0, 1), "{b}/{period}");
        }
    }
    let tokens = env.sql_rows("SELECT input_tokens, output_tokens FROM quota_usage WHERE bucket = 'total' AND period_type = 'daily'").await;
    assert_eq!(tokens[0].try_get_by_index::<i64>(0).unwrap(), 1000);
    assert_eq!(tokens[0].try_get_by_index::<i64>(1).unwrap(), 333);
    env.eventually("usage event", |e| e.usage_events().len() == 1).await;
    let u = env.usage_events()[0].clone();
    assert_eq!(u.billing_outcome, "completed");
    assert_eq!(u.settlement_method, "actual");
    assert_eq!(u.terminal_state, "completed");
    assert_eq!(u.actual_credits_micro, expected);
    assert_eq!(u.request_id, rid);
    assert_eq!(u.requester_type, "user");
    assert_eq!(u.effective_model, "gpt-4.1");
    assert_eq!(u.policy_version_applied, 1);
    assert_eq!(u.usage.unwrap().input_tokens, 1000);
    assert_eq!(u.dedupe_key.split('/').count(), 3);
    assert!(!u.dedupe_key.contains('-'), "hex-normalized ids: {}", u.dedupe_key);
    // the outbox payload carries no provider identifiers
    env.settle().await;
    assert_eq!(env.usage_events().len(), 1, "exactly one usage event per turn");
}

#[tokio::test]
async fn overshoot_beyond_tolerance_is_capped_and_turn_stays_completed() {
    let env = TestEnv::new().await;
    let chat = env.chat(Some("tiny-ctx")).await;
    env.proxy.respond(|r| {
        r.uri.contains("/responses").then(|| {
            sse_resp(&[
                ("response.output_text.delta".into(), json!({"delta": "long"})),
                ("response.completed".into(), json!({"response": {"usage": {"input_tokens": 9_000_000, "output_tokens": 1}}})),
            ])
        })
    });
    let r = env.send_msg(&chat, "x").await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    let reserved = env.count("SELECT reserved_credits_micro FROM chat_turns").await;
    let (spent, res, _) = bucket(&env, "total", "daily").await;
    assert_eq!(res, 0);
    assert_eq!(spent, reserved, "charge capped at the reserve");
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_turns WHERE state = 'completed'").await, 1);
}

#[tokio::test]
async fn provider_failure_without_usage_settles_estimated() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    env.proxy.respond(|r| r.uri.contains("/responses").then(|| crate::test_support::json_resp(500, &json!({}))));
    let r = env.send_msg(&chat, "x").await;
    assert_eq!(r.event_names().last().unwrap(), "error");
    // the provider was called: no free failure, estimated settlement (DESIGN §5.9 mapping table)
    let (spent, reserved, calls) = bucket(&env, "total", "daily").await;
    assert_eq!((reserved, calls), (0, 1));
    assert!(spent > 0);
    env.eventually("usage event", |e| e.usage_events().len() == 1).await;
    let u = &env.usage_events()[0];
    assert_eq!(u.billing_outcome, "failed");
    assert_eq!(u.settlement_method, "estimated");
    assert_eq!(u.actual_credits_micro, spent);
    assert!(u.usage.is_none());
}

#[tokio::test]
async fn premium_exhaustion_downgrades_then_rejects() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    env.send_msg(&chat, "warm up").await;
    env.sql_exec("UPDATE quota_usage SET spent_credits_micro = 50000000 WHERE bucket = 'tier:premium' AND period_type = 'daily'")
        .await;
    let r = env.send_msg(&chat, "next").await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    let done = r.event("done").unwrap();
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["selected_model"], "gpt-4.1");
    assert_eq!(done["effective_model"], "gpt-4.1-mini");
    assert_eq!(done["downgrade_from"], "gpt-4.1");
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");
    let warnings = done["quota_warnings"].as_array().unwrap();
    let premium_daily = warnings
        .iter()
        .find(|w| w["tier"] == "premium" && w["period"] == "daily")
        .unwrap();
    assert_eq!(premium_daily["exhausted"], true);
    assert_eq!(premium_daily["remaining_percentage"], 0);
    assert!(premium_daily["next_reset"].is_string());
    assert_eq!(env.proxy.chat_requests().last().unwrap().json()["model"], "gpt-4.1-mini");
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert_eq!(msgs["items"].as_array().unwrap().last().unwrap()["model"], "gpt-4.1-mini");
    // standard credits accounted only in total, not in the premium bucket
    let (premium_spent, _, _) = bucket(&env, "tier:premium", "daily").await;
    assert_eq!(premium_spent, 50_000_000);
    // all tiers exhausted → 429 tokens, no provider call, nothing persisted
    env.sql_exec("UPDATE quota_usage SET spent_credits_micro = 100000000 WHERE bucket = 'total' AND period_type = 'daily'")
        .await;
    let calls = env.proxy.chat_requests().len();
    let msgs_before = env.count("SELECT COUNT(*) FROM messages").await;
    let r = env.send_msg(&chat, "again").await;
    r.assert_problem(429, "quota_exceeded");
    let v = r.json();
    assert_eq!(v["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(v["context"]["violations"][0]["description"], "quota_exceeded");
    assert_eq!(env.proxy.chat_requests().len(), calls);
    assert_eq!(env.count("SELECT COUNT(*) FROM messages").await, msgs_before);
}

#[tokio::test]
async fn kill_switches_force_standard_and_disable_web_search() {
    let env = TestEnv::with(env_with_policy(json!({"kill_switches": {"force_standard_tier": true, "disable_web_search": true}}), json!({})))
        .await;
    let chat = env.chat(None).await;
    let r = env.send_msg(&chat, "x").await;
    let done = r.event("done").unwrap();
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_reason"], "force_standard_tier");
    let r = env.stream(&user_a(), &chat, json!({"content": "x", "web_search": {"enabled": true}})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(r.json()["context"]["violations"][0]["type"], "FEATURE_DISABLED");

    let env = TestEnv::with(env_with_policy(json!({"kill_switches": {"disable_premium_tier": true}}), json!({}))).await;
    let chat = env.chat(None).await;
    let done = env.send_msg(&chat, "x").await.event("done").unwrap();
    assert_eq!(done["downgrade_reason"], "disable_premium_tier");
}

#[tokio::test]
async fn disabled_chat_model_downgrades_with_model_disabled() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    env.sql_exec(&format!("UPDATE chats SET model = 'old-model' WHERE id = x'{}'", chat.replace('-', ""))).await;
    let r = env.send_msg(&chat, "x").await;
    let done = r.event("done").unwrap();
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_reason"], "model_disabled");
    assert_eq!(done["selected_model"], "old-model");
}

#[tokio::test]
async fn web_search_tool_citations_accounting_and_limits() {
    let env = TestEnv::with(env_with_policy(json!({}), json!({"quota": {"web_search_daily_quota": 2}}))).await;
    let chat = env.chat(None).await;
    // without the flag no tool is offered
    env.send_msg(&chat, "x").await;
    assert!(env.proxy.chat_requests().last().unwrap().json().get("tools").is_none());
    env.proxy.respond(|r| {
        r.uri.contains("/responses").then(|| {
            sse_resp(&[
                ("response.web_search_call.searching".into(), json!({"item_id": "ws_1"})),
                ("response.web_search_call.completed".into(), json!({"item_id": "ws_1"})),
                ("response.output_text.delta".into(), json!({"delta": "Per the site"})),
                (
                    "response.output_text.annotation.added".into(),
                    json!({"annotation": {"type": "url_citation", "url": "https://example.org/a", "title": "Example", "start_index": 0, "end_index": 3}}),
                ),
                ("response.completed".into(), json!({"response": {"usage": {"input_tokens": 10, "output_tokens": 3}}})),
            ])
        })
    });
    let r = env.stream(&user_a(), &chat, json!({"content": "search", "web_search": {"enabled": true}})).await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    let req = env.proxy.chat_requests().last().unwrap().json();
    let ws = req["tools"].as_array().unwrap().iter().find(|t| t["type"].as_str().unwrap().starts_with("web_search")).cloned();
    assert!(ws.is_some(), "web_search tool offered");
    assert_eq!(req["metadata"]["feature"], "web_search");
    assert!(req["instructions"].as_str().unwrap().len() > "You are a helpful assistant.".len(), "web search guidance appended");
    let tools: Vec<Value> = r.events().into_iter().filter(|(n, _)| n == "tool").map(|(_, d)| d).collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["phase"], "start");
    assert_eq!(tools[0]["name"], "web_search");
    assert_eq!(tools[1]["phase"], "done");
    let c = r.event("citations").unwrap();
    assert_eq!(c["items"][0]["source"], "web");
    assert_eq!(c["items"][0]["url"], "https://example.org/a");
    assert_eq!(c["items"][0]["title"], "Example");
    let calls = env.count("SELECT web_search_calls FROM quota_usage WHERE bucket = 'total' AND period_type = 'daily'").await;
    assert_eq!(calls, 1);
    env.eventually("usage", |e| e.usage_events().iter().any(|u| u.web_search_calls == 1)).await;
    // daily quota: one more allowed, then 429 web_search
    env.stream(&user_a(), &chat, json!({"content": "again", "web_search": {"enabled": true}})).await;
    let r = env.stream(&user_a(), &chat, json!({"content": "third", "web_search": {"enabled": true}})).await;
    r.assert_problem(429, "quota_exceeded");
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "web_search");
    // a request that does not use the tool is not rejected
    assert_eq!(env.send_msg(&chat, "plain").await.event_names().last().unwrap(), "done");
}

#[tokio::test]
async fn web_search_calls_exceeded_mid_turn() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    env.proxy.respond(|r| {
        r.uri.contains("/responses").then(|| {
            let mut ev = Vec::new();
            for i in 0..3 {
                ev.push(("response.web_search_call.searching".to_owned(), json!({"item_id": format!("ws_{i}")})));
                ev.push(("response.web_search_call.completed".to_owned(), json!({"item_id": format!("ws_{i}")})));
            }
            ev.push(("response.completed".into(), json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})));
            sse_resp(&ev)
        })
    });
    let r = env.stream(&user_a(), &chat, json!({"content": "s", "web_search": {"enabled": true}})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.event_names().last().unwrap(), "error");
    assert_eq!(r.event("error").unwrap()["code"], "web_search_calls_exceeded");
    let rows = env.sql_rows("SELECT state, error_code FROM chat_turns").await;
    assert_eq!(rows[0].try_get_by_index::<String>(0).unwrap(), "failed");
    assert_eq!(rows[0].try_get_by_index::<Option<String>>(1).unwrap().as_deref(), Some("web_search_calls_exceeded"));
    env.eventually("usage", |e| e.usage_events().len() == 1).await;
    let u = &env.usage_events()[0];
    assert_eq!(u.billing_outcome, "failed");
    assert_eq!(u.settlement_method, "estimated");
}

#[tokio::test]
async fn quota_status_endpoint_matches_usage() {
    let env = TestEnv::new().await;
    let r = env.get("/mini-chat/v1/quota/status").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["warning_threshold_pct"], 80);
    let tiers = v["tiers"].as_array().unwrap();
    let names: Vec<&str> = tiers.iter().map(|t| t["tier"].as_str().unwrap()).collect();
    assert!(names.contains(&"premium") && names.contains(&"total"), "{names:?}");
    for t in tiers {
        for p in t["periods"].as_array().unwrap() {
            assert_eq!(p["used_credits_micro"], 0);
            assert_eq!(p["remaining_percentage"], 100);
            assert_eq!(p["warning"], false);
            assert_eq!(p["exhausted"], false);
            assert!(p["next_reset"].as_str().unwrap().ends_with('Z') || p["next_reset"].as_str().unwrap().contains("+00:00"));
        }
    }
    let chat = env.chat(None).await;
    env.send_msg(&chat, "x").await;
    let spent = bucket(&env, "total", "daily").await.0;
    let v = env.get("/mini-chat/v1/quota/status").await.json();
    let total = v["tiers"].as_array().unwrap().iter().find(|t| t["tier"] == "total").unwrap().clone();
    let daily = total["periods"].as_array().unwrap().iter().find(|p| p["period"] == "daily").unwrap().clone();
    assert_eq!(daily["used_credits_micro"], spent);
    assert_eq!(daily["limit_credits_micro"], 100_000_000);
    assert_eq!(daily["remaining_credits_micro"], 100_000_000 - spent);
    // warning at 80% used
    env.sql_exec("UPDATE quota_usage SET spent_credits_micro = 85000000 WHERE bucket = 'total' AND period_type = 'daily'").await;
    let v = env.get("/mini-chat/v1/quota/status").await.json();
    let total = v["tiers"].as_array().unwrap().iter().find(|t| t["tier"] == "total").unwrap().clone();
    let daily = total["periods"].as_array().unwrap().iter().find(|p| p["period"] == "daily").unwrap().clone();
    assert_eq!(daily["remaining_percentage"], 15);
    assert_eq!(daily["warning"], true);
    assert_eq!(daily["exhausted"], false);
    for leak in ["billing", "settlement", "reserved"] {
        assert!(!v.to_string().contains(leak), "status leaks {leak}");
    }
    // other users see their own (empty) usage
    let other = env
        .call(&crate::test_support::ctx(crate::test_support::TENANT_A, crate::test_support::USER_A2), "GET", "/mini-chat/v1/quota/status", None)
        .await
        .json();
    let t = other["tiers"].as_array().unwrap().iter().find(|t| t["tier"] == "total").unwrap().clone();
    assert!(t["periods"].as_array().unwrap().iter().all(|p| p["used_credits_micro"] == 0));
}
