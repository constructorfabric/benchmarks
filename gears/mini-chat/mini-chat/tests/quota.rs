//! US6: quota enforcement, status, downgrade, credits, web search and code interpreter limits.
//!
//! AC: Quota Status API, Quota Enforcement (reserve-before-execute, downgrade, credits per
//! model/tier), Web Search, Settlement (exactly once), Principles (quota before provider).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::similar_names)]

mod common;
use std::time::Duration;

use common::*;
use http_body_util::BodyExt;
use mini_chat::domain::credits::credits_micro;
use serde_json::{Value, json};

fn limits(standard: i64, premium: i64) -> Value {
    json!({
        "default_standard_limits": {"limit_daily_credits_micro": standard, "limit_monthly_credits_micro": standard},
        "default_premium_limits": {"limit_daily_credits_micro": premium, "limit_monthly_credits_micro": premium}
    })
}

async fn harness_with(extra: Value) -> Harness {
    Harness::with(Opts { policy: policy_cfg(catalog(), extra), ..Default::default() }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credits_are_accounted_per_model_and_tier() {
    let h = Harness::new().await;
    let premium = h.chat(ALICE, Some("premium-m")).await;
    let ev = h.say(ALICE, premium, "x").await;
    assert_eq!(find(&ev, "done")["effective_model"], json!("premium-m"));
    let p_cost = credits_micro(42, 7, 3_000_000, 15_000_000).unwrap();
    assert_eq!(p_cost, 231);
    for period in ["daily", "monthly"] {
        let total = h.quota_row(ALICE, "total", period).await.unwrap();
        let tier = h.quota_row(ALICE, "tier:premium", period).await.unwrap();
        assert_eq!(total.spent_credits_micro, p_cost, "{period}");
        assert_eq!(tier.spent_credits_micro, p_cost, "{period}");
        assert_eq!(total.reserved_credits_micro, 0);
        assert_eq!(tier.reserved_credits_micro, 0);
    }
    let standard = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, standard, "y").await;
    let s_cost = credits_micro(42, 7, 1_000_000, 2_000_000).unwrap();
    assert_eq!(h.quota_row(ALICE, "total", "daily").await.unwrap().spent_credits_micro, p_cost + s_cost);
    assert_eq!(h.quota_row(ALICE, "tier:premium", "daily").await.unwrap().spent_credits_micro, p_cost, "standard turns never touch the premium bucket");
    h.eventually("usage", || h.usage_events().len() == 2).await;
    let mut costs: Vec<i64> = h.usage_events().iter().map(|u| u.actual_credits_micro).collect();
    costs.sort_unstable();
    assert_eq!(costs, vec![s_cost, p_cost]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_status_is_consistent_with_usage() {
    let h = harness_with(limits(1_000_000, 500_000)).await;
    let r = h.send(ALICE, "GET", "/quota/status", None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert!(v["warning_threshold_pct"].is_number());
    let tiers: Vec<&str> = v["tiers"].as_array().unwrap().iter().map(|t| t["tier"].as_str().unwrap()).collect();
    assert_eq!(tiers, vec!["premium", "total"]);
    for t in v["tiers"].as_array().unwrap() {
        let periods: Vec<&str> = t["periods"].as_array().unwrap().iter().map(|p| p["period"].as_str().unwrap()).collect();
        assert_eq!(periods, vec!["daily", "monthly"]);
        for p in t["periods"].as_array().unwrap() {
            assert_eq!(p["used_credits_micro"], json!(0));
            assert_eq!(p["remaining_percentage"], json!(100));
            assert_eq!(p["warning"], json!(false));
            assert_eq!(p["exhausted"], json!(false));
            assert!(p["next_reset"].is_string());
        }
    }
    let chat = h.chat(ALICE, Some("premium-m")).await;
    h.say(ALICE, chat, "x").await;
    let v = h.send(ALICE, "GET", "/quota/status", None).await.json();
    let get = |tier: &str, period: &str| -> Value {
        v["tiers"].as_array().unwrap().iter().find(|t| t["tier"] == json!(tier)).unwrap()["periods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["period"] == json!(period))
            .unwrap()
            .clone()
    };
    let total = get("total", "daily");
    assert_eq!(total["limit_credits_micro"], json!(1_000_000));
    assert_eq!(total["used_credits_micro"], json!(231));
    assert_eq!(total["remaining_credits_micro"], json!(1_000_000 - 231));
    assert_eq!(get("premium", "monthly")["limit_credits_micro"], json!(500_000));
    assert_eq!(get("premium", "monthly")["used_credits_micro"], json!(231));
    // Matches the database.
    let row = h.quota_row(ALICE, "total", "daily").await.unwrap();
    assert_eq!(total["used_credits_micro"], json!(row.spent_credits_micro + row.reserved_credits_micro));

    // Exhausted + warning flags.
    h.seed_spent(ALICE, "total", 1_000_000).await;
    let v = h.send(ALICE, "GET", "/quota/status", None).await.json();
    let total = v["tiers"].as_array().unwrap().iter().find(|t| t["tier"] == json!("total")).unwrap()["periods"][0].clone();
    assert_eq!(total["remaining_percentage"], json!(0));
    assert_eq!(total["exhausted"], json!(true));
    assert_eq!(total["warning"], json!(true));
    assert_eq!(total["remaining_credits_micro"], json!(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn premium_exhaustion_downgrades_to_standard() {
    let h = harness_with(limits(10_000_000, 100_000)).await;
    let chat = h.chat(ALICE, Some("premium-m")).await;
    h.seed_spent(ALICE, "tier:premium", 100_000).await;
    let ev = h.say(ALICE, chat, "x").await;
    let done = find(&ev, "done");
    assert_eq!(done["selected_model"], json!("premium-m"));
    assert_eq!(done["effective_model"], json!("standard-m"));
    assert_eq!(done["quota_decision"], json!("downgrade"));
    assert_eq!(done["downgrade_from"], json!("premium-m"));
    assert_eq!(done["downgrade_reason"], json!("premium_quota_exhausted"));
    let req = h.provider.chat_requests().pop().unwrap();
    assert_eq!(req["model"], json!("prov-standard-m"));
    // Charged at the standard rate, premium bucket untouched.
    assert_eq!(h.quota_row(ALICE, "total", "daily").await.unwrap().spent_credits_micro, 56);
    assert_eq!(h.quota_row(ALICE, "tier:premium", "daily").await.unwrap().spent_credits_micro, 100_000);
    // The chat model itself is unchanged; the assistant message records the effective model.
    assert_eq!(h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await.json()["model"], json!("premium-m"));
    assert_eq!(h.messages(chat).await[1].model.as_deref(), Some("standard-m"));
    // Replay rebuilds the downgrade outcome from stored models.
    let rid = h.turns(chat).await[0].request_id;
    let (_, ev) = h.stream(ALICE, chat, json!({"content": "x", "request_id": rid})).await;
    let done = find(&ev, "done");
    assert_eq!(done["quota_decision"], json!("downgrade"));
    assert_eq!(done["downgrade_from"], json!("premium-m"));
    assert!(done.get("downgrade_reason").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_switches_and_disabled_model_downgrade_with_reason() {
    for (ks, reason) in [
        (json!({"force_standard_tier": true}), "force_standard_tier"),
        (json!({"disable_premium_tier": true}), "disable_premium_tier"),
    ] {
        let h = Harness::new().await;
        let chat = h.chat(ALICE, Some("premium-m")).await;
        h.policy.set(&policy_cfg(catalog(), json!({"kill_switches": ks})));
        let ev = h.say(ALICE, chat, "x").await;
        let done = find(&ev, "done");
        assert_eq!(done["effective_model"], json!("standard-m"));
        assert_eq!(done["downgrade_reason"], json!(reason));
    }
    // The chat's model was disabled after creation.
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("premium-m")).await;
    let mut cat = catalog();
    cat[0]["enabled"] = json!(false);
    h.policy.set(&policy_cfg(cat, json!({})));
    let ev = h.say(ALICE, chat, "x").await;
    let done = find(&ev, "done");
    assert_eq!(done["quota_decision"], json!("downgrade"));
    assert_eq!(done["downgrade_reason"], json!("model_disabled"));
    assert_eq!(done["effective_model"], json!("standard-m"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_tiers_exhausted_is_429_tokens_without_provider_call() {
    let h = harness_with(limits(100_000, 100_000)).await;
    let chat = h.chat(ALICE, Some("premium-m")).await;
    h.seed_spent(ALICE, "total", 100_000).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x"}))).await;
    let p = r.problem(429);
    assert_eq!(p["context"]["violations"][0]["subject"], json!("tokens"));
    assert_eq!(p["context"]["violations"][0]["description"], json!("quota_exceeded"));
    assert!(h.provider.chat_requests().is_empty());
    assert!(h.turns(chat).await.is_empty());
    assert!(h.messages(chat).await.is_empty(), "no user message persisted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserve_is_held_during_the_turn_and_released_at_settlement() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::slow("a b c d", Duration::from_millis(250)));
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "x"})).await;
    let mut reserved = 0;
    for _ in 0..50 {
        if let Some(r) = h.quota_row(ALICE, "total", "daily").await {
            reserved = r.reserved_credits_micro;
            if reserved > 0 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let turn = h.turns(chat).await.pop().unwrap();
    assert_eq!(reserved, turn.reserved_credits_micro.unwrap(), "reserve written before the provider call");
    assert!(turn.reserve_tokens.unwrap() > 0);
    // Status counts the reserve as used while the turn runs.
    let v = h.send(ALICE, "GET", "/quota/status", None).await.json();
    let used = v["tiers"][1]["periods"][0]["used_credits_micro"].as_i64().unwrap();
    assert_eq!(used, reserved);
    resp.into_body().collect().await.unwrap();
    let row = h.quota_row(ALICE, "total", "daily").await.unwrap();
    assert_eq!(row.reserved_credits_micro, 0);
    assert_eq!(row.spent_credits_micro, credits_micro(5, 5, 1_000_000, 2_000_000).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_warnings_in_done_event() {
    let h = harness_with(limits(100_000, 100_000)).await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.seed_spent(ALICE, "total", 85_000).await;
    let ev = h.say(ALICE, chat, "x").await;
    let done = find(&ev, "done");
    let warnings = done["quota_warnings"].as_array().expect("quota_warnings");
    let w = warnings.iter().find(|w| w["tier"] == json!("total") && w["period"] == json!("daily")).expect("total daily warning");
    assert_eq!(w["warning"], json!(true));
    assert_eq!(w["exhausted"], json!(false));
    assert!(w["remaining_percentage"].as_u64().unwrap() <= 20);
    assert!(w["next_reset"].is_string());
    // No warnings far from the limit.
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let ev = h.say(ALICE, chat, "x").await;
    // Every period is listed with its flags; `next_reset` only when warning/exhausted.
    let ws = find(&ev, "done")["quota_warnings"].as_array().unwrap().clone();
    assert_eq!(ws.len(), 4, "{ws:?}");
    for w in &ws {
        assert_eq!(w["warning"], json!(false));
        assert_eq!(w["exhausted"], json!(false));
        assert!(w.get("next_reset").is_none_or(Value::is_null), "{w}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_search_tool_citations_and_accounting() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let answer = "Rust 2.0 was released.";
    h.provider.push(Reply::with_events(
        answer,
        vec![
            ("response.web_search_call.searching", json!({"type": "response.web_search_call.searching"})),
            ("response.web_search_call.completed", json!({"type": "response.web_search_call.completed"})),
        ],
        json!({"input_tokens": 20, "output_tokens": 10}),
        vec![json!({"type": "url_citation", "url": "https://example.com/rust", "title": "Rust news", "start_index": 0, "end_index": 8})],
    ));
    let (r, ev) = h.stream(ALICE, chat, json!({"content": "news?", "web_search": {"enabled": true}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let req = h.provider.chat_requests().pop().unwrap();
    let ws = req["tools"].as_array().unwrap().iter().find(|t| t["type"] == json!("web_search")).expect("web_search tool").clone();
    assert_eq!(ws["search_context_size"], json!("low"));
    assert_eq!(req["max_tool_calls"], json!(2));
    assert!(req["instructions"].as_str().unwrap().contains(mini_chat::config::DEFAULT_WEB_SEARCH_GUARD));
    assert_eq!(req["metadata"]["feature"], json!("web_search"));

    let tools: Vec<&Value> = ev.iter().filter(|(n, _)| n == "tool").map(|(_, d)| d).collect();
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().all(|t| t["name"] == json!("web_search")));
    let c = &find(&ev, "citations")["items"][0];
    assert_eq!(c["source"], json!("web"));
    assert_eq!(c["url"], json!("https://example.com/rust"));
    assert_eq!(c["title"], json!("Rust news"));
    assert_eq!(c["snippet"], json!("Rust 2.0"));
    assert_eq!(c["span"], json!({"start": 0, "end": 8}));

    let turn = h.turns(chat).await.pop().unwrap();
    assert!(turn.web_search_enabled);
    assert_eq!(turn.web_search_completed_count, 1);
    assert_eq!(h.quota_row(ALICE, "total", "daily").await.unwrap().web_search_calls, 1);
    h.eventually("usage", || h.usage_events().len() == 1).await;
    assert_eq!(h.usage_events()[0].web_search_calls, 1);

    // Without the flag the tool is not offered.
    h.say(ALICE, chat, "no search").await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req.get("tools").is_none_or(|t| !t.to_string().contains("web_search")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_search_per_message_limit_and_daily_quota_and_kill_switch() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let max = h.cfg.quota.web_search_max_calls_per_message;
    let mut pre = Vec::new();
    for _ in 0..=max {
        pre.push(("response.web_search_call.searching", json!({"type": "response.web_search_call.searching"})));
        pre.push(("response.web_search_call.completed", json!({"type": "response.web_search_call.completed"})));
    }
    h.provider.push(Reply::with_events("x", pre, json!({"input_tokens": 1, "output_tokens": 1}), vec![]));
    let (_, ev) = h.stream(ALICE, chat, json!({"content": "q", "web_search": {"enabled": true}})).await;
    assert_eq!(find(&ev, "error")["code"], json!("web_search_calls_exceeded"));
    assert_eq!(h.turns(chat).await[0].error_code.as_deref(), Some("web_search_calls_exceeded"));

    // Daily quota.
    let daily = i64::from(h.cfg.quota.web_search_daily_quota);
    h.seed_daily_tool_calls(ALICE, daily, 0).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "q", "web_search": {"enabled": true}}))).await;
    let p = r.problem(429);
    assert_eq!(p["context"]["violations"][0]["subject"], json!("web_search"));
    // Without web search the user can still chat.
    h.say(ALICE, chat, "plain").await;

    // Kill switch.
    h.policy.set(&policy_cfg(catalog(), json!({"kill_switches": {"disable_web_search": true}})));
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "q", "web_search": {"enabled": true}}))).await;
    let p = r.problem(400);
    assert_eq!(p["context"]["violations"][0]["type"], json!("FEATURE_DISABLED"));
    assert_eq!(p["context"]["violations"][0]["subject"], json!("web_search"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_interpreter_daily_quota() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let r = h.upload(ALICE, chat, "t.xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", b"PK").await;
    assert_eq!(r.status, 201);
    let daily = i64::from(h.cfg.quota.code_interpreter_daily_quota);
    h.seed_daily_tool_calls(ALICE, 0, daily).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "sum"}))).await;
    let p = r.problem(429);
    assert_eq!(p["context"]["violations"][0]["subject"], json!("code_interpreter"));
    assert!(h.provider.chat_requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_publish_is_retried_and_delivered_exactly_once() {
    let h = Harness::new().await;
    h.policy.publish_failures.store(2, std::sync::atomic::Ordering::SeqCst);
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "x").await;
    for _ in 0..400 {
        if !h.usage_events().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(h.usage_events().len(), 1, "delivered after transient failures");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.usage_events().len(), 1, "exactly once");
    let u = &h.usage_events()[0];
    assert!(!u.dedupe_key.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn images_on_turn_downgraded_to_non_vision_model_are_rejected() {
    // Premium has vision; the only standard model does not.
    let cat = json!([
        model("premium-m", "Premium", true, true, json!({})),
        model("novision-m", "Standard", true, false, json!({"multimodal_capabilities": []})),
    ]);
    let h = Harness::with(Opts { policy: policy_cfg(cat, limits(10_000_000, 100_000)), ..Default::default() }).await;
    let chat = h.chat(ALICE, Some("premium-m")).await;
    let img = h.upload(ALICE, chat, "a.png", "image/png", &png(2, 2)).await;
    assert_eq!(img.status, 201, "{}", img.text());
    let img = img.json()["id"].as_str().unwrap().to_owned();
    h.seed_spent(ALICE, "tier:premium", 100_000).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "look", "attachment_ids": [img]}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "VISION_NOT_SUPPORTED");
    assert!(h.provider.chat_requests().is_empty());
    assert!(h.turns(chat).await.is_empty());
    // Without the image the downgraded turn runs.
    let ev = h.say(ALICE, chat, "text only").await;
    assert_eq!(find(&ev, "done")["effective_model"], json!("novision-m"));
}
