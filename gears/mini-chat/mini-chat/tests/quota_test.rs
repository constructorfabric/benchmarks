//! Reserve-before-execute, tier downgrade cascade, kill switches, credit
//! arithmetic per model/tier, settlement exactly once per terminal outcome,
//! usage published once.
#![allow(clippy::type_complexity)]

mod common;

use common::*;
use futures::StreamExt;
use http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn rejected_preflight_makes_no_provider_call() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.policy.set_limits((1, 1), (1, 1));
    let s = h.send(&a, chat, "hi").await;
    assert_eq!(s.status, StatusCode::TOO_MANY_REQUESTS, "{}", s.raw);
    assert_eq!(s.problem["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.resource_exhausted.v1~");
    assert_eq!(s.problem["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(s.problem["context"]["violations"][0]["description"], "quota_exceeded");
    assert!(h.gw.chat_calls().is_empty());
    assert_eq!(h.scalar("SELECT COUNT(*) FROM chat_turns").await, 0);
    assert_eq!(h.scalar("SELECT COUNT(*) FROM messages").await, 0);
    assert_eq!(h.quota(USER_A, "total", "daily").await, (0, 0, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn credits_are_settled_per_model_multipliers() {
    let h = Harness::new().await;
    let a = user_a();
    // premium: in 1.0/token, out 3.0/token (micro-credits per token at 1e6/1e6)
    let chat = h.create_chat(&a).await;
    h.gw.push(text_reply(&["x"], 120, 30));
    let s = h.send(&a, chat, "hi").await;
    assert_eq!(s.names().last(), Some(&"done"));
    let expect_premium = 120 + 3 * 30;
    assert_eq!(h.quota(USER_A, "total", "daily").await, (expect_premium, 0, 1));
    assert_eq!(h.quota(USER_A, "total", "monthly").await, (expect_premium, 0, 1));
    assert_eq!(h.quota(USER_A, "tier:premium", "daily").await, (expect_premium, 0, 1));
    assert_eq!(h.quota(USER_A, "tier:premium", "monthly").await, (expect_premium, 0, 1));

    // standard: in 0.5/token, out 1.0/token; does not touch the premium bucket
    let std_chat = h.create_chat_with(&a, json!({"model": "gpt-standard"})).await;
    let std_chat = Uuid::parse_str(std_chat.body["id"].as_str().unwrap()).unwrap();
    h.gw.push(text_reply(&["y"], 121, 30));
    h.send(&a, std_chat, "hi").await;
    let expect_std = 61 + 30; // ceil(121 * 0.5) + 30
    assert_eq!(h.quota(USER_A, "total", "daily").await, (expect_premium + expect_std, 0, 2));
    assert_eq!(h.quota(USER_A, "tier:premium", "daily").await, (expect_premium, 0, 1));
    // token telemetry on the total bucket
    let tel = h
        .rows(&format!(
            "SELECT CAST(input_tokens AS TEXT), CAST(output_tokens AS TEXT) FROM quota_usage \
             WHERE user_id = {} AND bucket = 'total' AND period_type = 'daily'",
            blob(USER_A)
        ))
        .await;
    assert_eq!(tel[0][0].as_deref(), Some("241"));
    assert_eq!(tel[0][1].as_deref(), Some("60"));

    // quota status reflects the same usage
    let st = h.get("/mini-chat/v1/quota/status", &a).await;
    let total = st.body["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tier"] == "total")
        .unwrap();
    let daily = total["periods"].as_array().unwrap().iter().find(|p| p["period"] == "daily").unwrap();
    assert_eq!(daily["used_credits_micro"], expect_premium + expect_std);
}

#[tokio::test(flavor = "multi_thread")]
async fn reserve_is_held_while_running_and_released_on_settlement() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let tx = h.gw.push_channel();
    tx.send(created_frame()).unwrap();
    let rid = Uuid::new_v4();
    let resp = h
        .open_stream(
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            &a,
            json!({"content": "hello", "request_id": rid}),
        )
        .await;
    let mut body = resp.into_body().into_data_stream();
    let mut seen = String::new();
    while let Some(Ok(c)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&c));
        if seen.contains("stream_started") {
            break;
        }
    }
    let (spent, reserved, _) = h.quota(USER_A, "total", "daily").await;
    assert_eq!(spent, 0);
    assert!(reserved > 0, "reserve booked before the provider call");
    let row = h.turn_row(rid).await;
    let reserve_tokens: i64 = row[2].as_deref().unwrap().parse().unwrap();
    let reserved_credits: i64 = row[3].as_deref().unwrap().parse().unwrap();
    assert!(reserve_tokens >= 1000, "includes max_output_tokens_applied");
    assert_eq!(reserved_credits, reserved);
    tx.send(delta_frame("ok")).unwrap();
    tx.send(completed_frame(10, 2)).unwrap();
    drop(tx);
    while body.next().await.is_some() {}
    assert_eq!(h.quota(USER_A, "total", "daily").await, (10 + 6, 0, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn downgrade_when_premium_exhausted() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    // premium sub-cap too small for the reserve; total cap is large
    h.policy.set_limits((100_000_000, 1_000_000_000), (10, 10));
    let s = h.send(&a, chat, "hi").await;
    assert_eq!(s.names().last(), Some(&"done"), "{}", s.raw);
    let done = s.first("done").unwrap();
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["selected_model"], "gpt-premium");
    assert_eq!(done["effective_model"], "gpt-standard");
    assert_eq!(done["downgrade_from"], "gpt-premium");
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");
    let call = h.gw.chat_calls().pop().unwrap().json();
    assert_eq!(call["model"], "gpt-standard-provider");
    let msgs = h.messages(&a, chat).await;
    assert_eq!(msgs[1]["model"], "gpt-standard");
    // settled against the standard model and not the premium bucket
    assert_eq!(h.quota(USER_A, "tier:premium", "daily").await.0, 0);
    assert!(h.quota(USER_A, "total", "daily").await.0 > 0);
    // replay rebuilds the decision without the reason
    let replay = h
        .send_body(&a, chat, json!({"content": "hi", "request_id": s.request_id()}))
        .await;
    let rd = replay.first("done").unwrap();
    assert_eq!(rd["quota_decision"], "downgrade");
    assert_eq!(rd["downgrade_from"], "gpt-premium");
    assert!(rd.get("downgrade_reason").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_switches_and_disabled_model_downgrade() {
    let h = Harness::new().await;
    let a = user_a();
    let cases: Vec<(&str, Box<dyn Fn(&mut mini_chat_sdk::PolicySnapshot)>)> = vec![
        ("force_standard_tier", Box::new(|s| s.kill_switches.force_standard_tier = true)),
        ("disable_premium_tier", Box::new(|s| s.kill_switches.disable_premium_tier = true)),
        (
            "model_disabled",
            Box::new(|s| {
                for m in &mut s.model_catalog {
                    if m.id == "gpt-premium" {
                        m.enabled = false;
                    }
                }
            }),
        ),
    ];
    for (reason, f) in cases {
        let chat = h.create_chat(&a).await;
        let before = h.policy.current();
        h.policy.update(|s| f(s));
        let s = h.send(&a, chat, "hi").await;
        let done = s.first("done").unwrap_or_else(|| panic!("{reason}: {}", s.raw));
        assert_eq!(done["quota_decision"], "downgrade", "{reason}");
        assert_eq!(done["effective_model"], "gpt-standard", "{reason}");
        assert_eq!(done["downgrade_reason"], reason);
        h.policy.update(|s| *s = mini_chat_sdk::PolicySnapshot { policy_version: s.policy_version, ..before.clone() });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_published_exactly_once_per_turn() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.gw.push(text_reply(&["x"], 100, 10));
    let ok = h.send(&a, chat, "one").await;
    h.gw.push(Reply::Status(500, json!({"error": {"message": "x"}})));
    let failed = h.send(&a, chat, "two").await;
    h.drain_outbox().await;
    let events = h.policy.published.lock().clone();
    assert_eq!(events.len(), 2, "{events:?}");
    let by_rid = |rid: Uuid| -> Vec<&mini_chat_sdk::UsageEvent> { events.iter().filter(|e| e.request_id == rid).collect() };
    let e = by_rid(ok.request_id());
    assert_eq!(e.len(), 1);
    let e = e[0];
    assert_eq!(e.terminal_state, "completed");
    assert_eq!(e.billing_outcome, "completed");
    assert_eq!(e.settlement_method, "actual");
    assert_eq!(e.actual_credits_micro, 130);
    assert_eq!(e.effective_model, "gpt-premium");
    assert_eq!(e.selected_model, "gpt-premium");
    assert_eq!(e.user_id, Some(USER_A));
    assert_eq!(e.tenant_id, TENANT_A);
    assert_eq!(e.requester_type, "user");
    assert_eq!(e.usage.unwrap().input_tokens, 100);
    assert!(!e.dedupe_key.is_empty());
    let f = by_rid(failed.request_id());
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].terminal_state, "failed");
    // all reserves released
    assert_eq!(h.quota(USER_A, "total", "daily").await.1, 0);
    assert_eq!(h.quota(USER_A, "total", "daily").await.2, 2, "one settlement per turn");
    let keys: std::collections::HashSet<_> = events.iter().map(|e| e.dedupe_key.clone()).collect();
    assert_eq!(keys.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_publish_is_retried_until_accepted() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.policy.publish_fail.store(true, std::sync::atomic::Ordering::SeqCst);
    h.send(&a, chat, "one").await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(h.policy.published.lock().is_empty());
    h.policy.publish_fail.store(false, std::sync::atomic::Ordering::SeqCst);
    h.eventually("usage delivered after retry", || async {
        (h.policy.published.lock().len() == 1).then_some(())
    })
    .await;
    h.drain_outbox().await;
    assert_eq!(h.policy.published.lock().len(), 1, "exactly once");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_turn_settles_estimated() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let tx = h.gw.push_channel();
    tx.send(created_frame()).unwrap();
    tx.send(delta_frame("some text")).unwrap();
    let rid = Uuid::new_v4();
    let resp = h
        .open_stream(
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            &a,
            json!({"content": "hello", "request_id": rid}),
        )
        .await;
    let mut body = resp.into_body().into_data_stream();
    let mut seen = String::new();
    while let Some(Ok(c)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&c));
        if seen.contains("some text") {
            break;
        }
    }
    drop(body);
    h.eventually("cancelled", || async {
        (h.turn_row(rid).await[0].as_deref() == Some("cancelled")).then_some(())
    })
    .await;
    h.drain_outbox().await;
    let (spent, reserved, calls) = h.quota(USER_A, "total", "daily").await;
    assert_eq!(reserved, 0);
    assert_eq!(calls, 1);
    assert!(spent > 0, "estimated settlement debits a bounded amount");
    let ev: Vec<Value> = h
        .policy
        .published
        .lock()
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["terminal_state"], "cancelled");
    assert_eq!(ev[0]["settlement_method"], "estimated");
    assert_eq!(ev[0]["billing_outcome"], "aborted");
    drop(tx);
}

#[tokio::test(flavor = "multi_thread")]
async fn overshoot_beyond_tolerance_is_capped_at_reserve() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    // reserve ≈ 112 input + 1000 output tokens; actual 5000 + 900 tokens
    h.gw.push(text_reply(&["long"], 5000, 900));
    let s = h.send(&a, chat, "hi").await;
    assert_eq!(s.names().last(), Some(&"done"), "the completed response stays completed");
    let row = h.turn_row(s.request_id()).await;
    let reserved: i64 = row[3].as_deref().unwrap().parse().unwrap();
    let (spent, held, _) = h.quota(USER_A, "total", "daily").await;
    assert_eq!(spent, reserved, "committed credits capped at the reserve");
    assert_eq!(held, 0);
    // within tolerance the actual amount is committed even if above the estimate
    let chat2 = h.create_chat(&a).await;
    h.gw.push(text_reply(&["ok"], 150, 1000));
    let s2 = h.send(&a, chat2, "hi").await;
    let r2: i64 = h.turn_row(s2.request_id()).await[3].as_deref().unwrap().parse().unwrap();
    let (spent2, _, _) = h.quota(USER_A, "total", "daily").await;
    assert_eq!(spent2 - spent, 150 + 3000);
    assert!(150 + 3000 > r2, "actual above the reserve but within tolerance");
}
