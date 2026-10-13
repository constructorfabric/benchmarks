//! T059/T060: models API, quota status, downgrade cascade, credits per model/tier.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use serde_json::{Value, json};
use tokio::sync::Notify;

fn period<'a>(v: &'a Value, tier: &str, period: &str) -> &'a Value {
    v["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tier"] == tier)
        .unwrap_or_else(|| panic!("tier {tier} in {v}"))["periods"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["period"] == period)
        .unwrap()
}

#[tokio::test]
async fn models_list_and_get() {
    let h = Harness::new().await;
    let (s, v) = h.get("/mini-chat/v1/models").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["items"].as_array().unwrap();
    let ids: Vec<&str> = items
        .iter()
        .map(|m| m["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["prem", "std"], "enabled only, catalog order");
    let prem = &items[0];
    assert_eq!(prem["display_name"], "Model prem");
    assert_eq!(prem["tier"], "premium");
    assert_eq!(prem["multiplier_display"], "1x");
    assert_eq!(prem["description"], "A test model");
    assert_eq!(prem["multimodal_capabilities"], json!(["VISION_INPUT"]));
    assert_eq!(prem["context_window"], 128_000);
    let keys: Vec<&String> = prem.as_object().unwrap().keys().collect();
    for internal in [
        "provider_model_id",
        "provider_id",
        "system_prompt",
        "input_tokens_credit_multiplier_micro",
        "enabled",
        "estimation_budgets",
    ] {
        assert!(
            !keys.iter().any(|k| *k == internal),
            "{internal} leaked: {prem}"
        );
    }
    assert!(
        items[1].get("description").is_none(),
        "empty description omitted"
    );

    let (s, v) = h.get("/mini-chat/v1/models/std").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["tier"], "standard");
    for id in ["off", "nope"] {
        let (s, v) = h.get(&format!("/mini-chat/v1/models/{id}")).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(
            v["context"]["resource_type"]
                .as_str()
                .unwrap()
                .contains("mini_chat.model"),
            "{v}"
        );
    }
}

#[tokio::test]
async fn quota_status_reflects_usage() {
    let h = Harness::new().await;
    let (s, v) = h.get("/mini-chat/v1/quota/status").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let total = period(&v, "total", "daily");
    assert_eq!(total["limit_credits_micro"], 100_000_000);
    assert_eq!(total["used_credits_micro"], 0);
    assert_eq!(total["remaining_percentage"], 100);
    assert_eq!(total["warning"], false);
    assert_eq!(total["exhausted"], false);
    assert!(total["next_reset"].is_string());
    assert_eq!(
        period(&v, "premium", "monthly")["limit_credits_micro"],
        500_000_000
    );
    assert!(v["warning_threshold_pct"].is_number());

    let chat = h.create_chat().await;
    // within the reserve (13 estimated input + 1000 max output tokens): actual usage is charged
    h.provider.push(Script::Ok {
        parts: vec!["x".into()],
        usage: (5, 500),
        before_done: vec![],
        output: None,
    });
    h.send(chat, "x").await;
    let (_, v) = h.get("/mini-chat/v1/quota/status").await;
    let used = 5 + 2 * 500;
    for tier in ["total", "premium"] {
        for p in ["daily", "monthly"] {
            assert_eq!(
                period(&v, tier, p)["used_credits_micro"],
                used,
                "{tier}/{p}: {v}"
            );
        }
    }
    let t = period(&v, "total", "daily");
    assert_eq!(t["remaining_credits_micro"], 100_000_000 - used);

    // reserved credits count as used while a turn is running
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["x".into()],
        gate: gate.clone(),
        usage: (1, 1),
    });
    let mut s = h.open_send(chat, json!({"content": "y"})).await;
    assert!(s.until("delta").await);
    let (_, v) = h.get("/mini-chat/v1/quota/status").await;
    assert!(
        period(&v, "total", "daily")["used_credits_micro"]
            .as_i64()
            .unwrap()
            > used + 2000
    );
    gate.notify_one();
    assert!(s.until("done").await);
}

#[tokio::test]
async fn quota_warning_and_exhaustion_flags() {
    let h = Harness::new().await;
    let mut p = default_policy();
    // reserve for "x" on prem = 13 + 2*1000 = 2013 credits
    p["default_premium_limits"] =
        json!({"limit_daily_credits_micro": 2100, "limit_monthly_credits_micro": 1_000_000});
    h.set_policy(p.clone());
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["x".into()],
        usage: (13, 1000),
        before_done: vec![],
        output: None,
    });
    let r = h.send(chat, "x").await;
    let w = r.first("done").unwrap()["quota_warnings"]
        .as_array()
        .unwrap()
        .clone();
    let pd = w
        .iter()
        .find(|x| x["tier"] == "premium" && x["period"] == "daily")
        .unwrap();
    assert_eq!(pd["warning"], true, "{w:?}");
    assert_eq!(pd["exhausted"], false);
    let td = w
        .iter()
        .find(|x| x["tier"] == "total" && x["period"] == "daily")
        .unwrap();
    assert_eq!(td["warning"], false);

    let h = Harness::new().await;
    p["default_premium_limits"] =
        json!({"limit_daily_credits_micro": 2013, "limit_monthly_credits_micro": 1_000_000});
    h.set_policy(p);
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["x".into()],
        usage: (13, 1000),
        before_done: vec![],
        output: None,
    });
    let r = h.send(chat, "x").await;
    assert_eq!(r.first("done").unwrap()["quota_decision"], "allow");
    let (_, v) = h.get("/mini-chat/v1/quota/status").await;
    let pd = period(&v, "premium", "daily");
    assert_eq!(pd["exhausted"], true, "{v}");
    assert_eq!(pd["remaining_credits_micro"], 0);
    assert_eq!(pd["remaining_percentage"], 0);
}

#[tokio::test]
async fn premium_exhausted_downgrades_to_standard() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let mut p = default_policy();
    p["default_premium_limits"] =
        json!({"limit_daily_credits_micro": 10, "limit_monthly_credits_micro": 10});
    h.set_policy(p);
    let r = h.send(chat, "x").await;
    let done = r.first("done").unwrap();
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["selected_model"], "prem");
    assert_eq!(done["effective_model"], "std");
    assert_eq!(done["downgrade_from"], "prem");
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");
    let req = h.provider.chat_requests().pop().unwrap();
    assert_eq!(req["model"], "prov-std");
    let msgs = h.messages(chat).await;
    assert_eq!(msgs[1]["model"], "std");
    // chat model is unchanged
    let (_, c) = h.get(&format!("/mini-chat/v1/chats/{chat}")).await;
    assert_eq!(c["model"], "prem");
    // premium bucket untouched, total charged
    let rows = db::quota_rows(&h).await;
    assert!(db::premium_daily(&rows).is_none_or(|r| r.spent_credits_micro == 0));
    assert!(db::total_daily(&rows).spent_credits_micro > 0);
    // replay rebuilds the downgrade without a reason
    let rid = r.request_id();
    let rep = h
        .send_body(chat, json!({"content": "x", "request_id": rid}))
        .await;
    let d = rep.first("done").unwrap();
    assert_eq!(d["quota_decision"], "downgrade");
    assert_eq!(d["downgrade_from"], "prem");
    assert!(d.get("downgrade_reason").is_none());
}

#[tokio::test]
async fn kill_switch_downgrade_reasons() {
    for (switch, reason) in [
        ("force_standard_tier", "force_standard_tier"),
        ("disable_premium_tier", "disable_premium_tier"),
    ] {
        let h = Harness::new().await;
        let chat = h.create_chat().await;
        let mut p = default_policy();
        p["kill_switches"] = json!({switch: true});
        h.set_policy(p);
        let r = h.send(chat, "x").await;
        let done = r.first("done").unwrap();
        assert_eq!(done["effective_model"], "std");
        assert_eq!(done["downgrade_reason"], reason);
    }
    // disabled selected model -> model_disabled
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let mut p = default_policy();
    p["model_catalog"][0]["enabled"] = json!(false);
    let mut prem2 = model("prem2", "premium", true, true, (true, true, true));
    prem2["preference"]["is_default"] = json!(true);
    p["model_catalog"].as_array_mut().unwrap().push(prem2);
    h.set_policy(p);
    let r = h.send(chat, "x").await;
    let done = r.first("done").unwrap();
    assert_eq!(done["effective_model"], "prem2");
    assert_eq!(done["downgrade_reason"], "model_disabled");
}

#[tokio::test]
async fn credits_use_model_multipliers() {
    let h = Harness::new().await;
    let mut p = default_policy();
    p["model_catalog"][1]["input_tokens_credit_multiplier_micro"] = json!(2_500_000);
    p["model_catalog"][1]["output_tokens_credit_multiplier_micro"] = json!(3_000_000);
    h.set_policy(p);
    let chat = h.create_chat_as(&h.ctx(), json!({"model": "std"})).await;
    h.provider.push(Script::Ok {
        parts: vec!["x".into()],
        usage: (101, 33),
        before_done: vec![],
        output: None,
    });
    let rid = h.send(chat, "x").await.request_id();
    // ceil(101*2.5) + 33*3
    let expected = 253 + 99;
    let rows = db::quota_rows(&h).await;
    let t = db::total_daily(&rows);
    assert_eq!(t.spent_credits_micro, expected);
    assert_eq!(t.reserved_credits_micro, 0);
    assert_eq!(t.input_tokens, 101);
    assert_eq!(t.output_tokens, 33);
    assert_eq!(t.calls, 1);
    assert!(
        db::premium_daily(&rows).is_none_or(|r| r.spent_credits_micro == 0),
        "standard model never charges premium"
    );
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    assert_eq!(h.published_for(rid)[0].actual_credits_micro, expected);
}

#[tokio::test]
async fn reserved_credits_block_parallel_overspend() {
    let h = Harness::new().await;
    let mut p = default_policy();
    // enough for exactly one reserve (~est_input + 2*1000) on both tiers
    p["default_standard_limits"] =
        json!({"limit_daily_credits_micro": 3000, "limit_monthly_credits_micro": 3000});
    p["default_premium_limits"] =
        json!({"limit_daily_credits_micro": 3000, "limit_monthly_credits_micro": 3000});
    h.set_policy(p);
    let a = h.create_chat().await;
    let b = h.create_chat().await;
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["x".into()],
        gate: gate.clone(),
        usage: (1, 1),
    });
    let mut s = h.open_send(a, json!({"content": "x"})).await;
    assert!(s.until("delta").await);
    let r = h.send(b, "y").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{r:?}");
    gate.notify_one();
    assert!(s.until("done").await);
    h.wait_turn_terminal(a).await;
    // after settlement (3 credits actual) the reserve is released again
    let r = h.send(b, "y").await;
    assert_eq!(r.status, StatusCode::OK);
}
