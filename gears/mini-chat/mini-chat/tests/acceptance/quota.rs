//! Quota status, enforcement, settlement, usage publication and web search.

use std::sync::atomic::Ordering;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::*;

fn period<'a>(status: &'a Value, tier: &str, period: &str) -> &'a Value {
    status["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tier"] == tier)
        .unwrap_or_else(|| panic!("tier {tier}"))["periods"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["period"] == period)
        .unwrap()
}

fn rid_of(r: &Resp) -> Uuid {
    Uuid::parse_str(r.event("stream_started").unwrap()["request_id"].as_str().unwrap()).unwrap()
}

fn close(tx: &tokio::sync::mpsc::UnboundedSender<bytes::Bytes>) {
    let _ = tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into());
}

/// Quota status reporting is accurate and consistent with actual usage.
#[tokio::test]
async fn quota_status_accurate_and_consistent() {
    let h = Harness::new().await;
    let s = h.call(U1, "GET", "/quota/status", None).await;
    assert_eq!(s.status, 200, "{}", s.text());
    let s = s.json();
    assert_eq!(s["warning_threshold_pct"], 80);
    let tiers: Vec<&str> = s["tiers"].as_array().unwrap().iter().map(|t| t["tier"].as_str().unwrap()).collect();
    assert_eq!(tiers, vec!["premium", "total"]);
    let pd = period(&s, "premium", "daily");
    assert_eq!(pd["limit_credits_micro"], 50_000_000);
    assert_eq!(pd["used_credits_micro"], 0);
    assert_eq!(pd["remaining_credits_micro"], 50_000_000);
    assert_eq!(pd["remaining_percentage"], 100);
    assert_eq!(pd["warning"], false);
    assert_eq!(pd["exhausted"], false);
    assert_eq!(period(&s, "total", "monthly")["limit_credits_micro"], 1_000_000_000);
    let now = time::OffsetDateTime::now_utc();
    let tomorrow = (now.date() + time::Duration::days(1)).midnight().assume_utc();
    let reset = time::OffsetDateTime::parse(pd["next_reset"].as_str().unwrap(), &time::format_description::well_known::Rfc3339).unwrap();
    assert_eq!(reset, tomorrow);
    let mreset = time::OffsetDateTime::parse(
        period(&s, "premium", "monthly")["next_reset"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert_eq!(mreset.day(), 1);
    assert!(mreset > now);

    // After a premium turn: 10 * 3 + 5 * 15 = 105 in both buckets.
    let chat = h.create_chat(U1, None).await;
    h.send_message(U1, chat, json!({"content": "hi"})).await;
    let s = h.call(U1, "GET", "/quota/status", None).await.json();
    for tier in ["premium", "total"] {
        for p in ["daily", "monthly"] {
            assert_eq!(period(&s, tier, p)["used_credits_micro"], 105, "{tier} {p}");
        }
    }
    assert_eq!(period(&s, "premium", "daily")["remaining_credits_micro"], 50_000_000 - 105);
    // Standard turn counts only in total.
    let std_chat = h.create_chat(U1, Some("standard-1")).await;
    h.send_message(U1, std_chat, json!({"content": "hi"})).await; // 10 + 15 = 25
    let s = h.call(U1, "GET", "/quota/status", None).await.json();
    assert_eq!(period(&s, "premium", "daily")["used_credits_micro"], 105);
    assert_eq!(period(&s, "total", "daily")["used_credits_micro"], 130);
    // Used includes in-flight reserves (conservative).
    let (tx, _d) = h.provider.push_channel();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{std_chat}/messages:stream"), Some(json!({"content": "x"}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    let reserved = h.turns(std_chat).await.last().unwrap().reserved_credits_micro.unwrap();
    assert!(reserved > 0);
    let s = h.call(U1, "GET", "/quota/status", None).await.json();
    assert_eq!(period(&s, "total", "daily")["used_credits_micro"].as_i64().unwrap(), 130 + reserved);
    close(&tx);
    assert!(read_until(&mut body, &mut buf, |n, _| n == "done").await);
    // Warning and exhaustion flags.
    h.seed_spent(TENANT_A, USER_1, "tier:premium", "daily", 45_000_000).await;
    h.seed_spent(TENANT_A, USER_1, "tier:premium", "monthly", 500_000_000).await;
    let s = h.call(U1, "GET", "/quota/status", None).await.json();
    let pd = period(&s, "premium", "daily");
    assert_eq!(pd["remaining_percentage"], 10);
    assert_eq!(pd["warning"], true);
    assert_eq!(pd["exhausted"], false);
    let pm = period(&s, "premium", "monthly");
    assert_eq!(pm["remaining_percentage"], 0);
    assert_eq!(pm["exhausted"], true);
    assert_eq!(pm["warning"], true);
    // done.quota_warnings mirrors the status.
    let r = h.send_message(U1, std_chat, json!({"content": "y"})).await;
    let w = r.event("done").unwrap()["quota_warnings"].clone();
    let pm = w.as_array().unwrap().iter().find(|x| x["tier"] == "premium" && x["period"] == "monthly").unwrap();
    assert_eq!(pm["exhausted"], true);
    assert!(pm["next_reset"].is_string());
    // Users are isolated.
    let other = h.call(U2, "GET", "/quota/status", None).await.json();
    assert_eq!(period(&other, "total", "daily")["used_credits_micro"], 0);
}

/// Reserve-before-execute enforced on every provider call.
#[tokio::test]
async fn reserve_before_execute() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let (tx, _d) = h.provider.push_channel();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "hello"}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    let turn = h.turns(chat).await.pop().unwrap();
    let reserved = turn.reserved_credits_micro.unwrap();
    assert!(turn.reserve_tokens.unwrap() > 0);
    assert_eq!(turn.max_output_tokens_applied, Some(1000));
    assert_eq!(turn.effective_model.as_deref(), Some("premium-1"));
    for bucket in ["total", "tier:premium"] {
        for p in ["daily", "monthly"] {
            let q = h.quota(USER_1, bucket, p).await.unwrap();
            assert_eq!(q.reserved_credits_micro, reserved, "{bucket} {p}");
            assert_eq!(q.spent_credits_micro, 0);
        }
    }
    // The request was sent only after the reserve was written.
    assert_eq!(h.provider.chat_requests().len(), 1);
    assert_eq!(h.provider.chat_requests()[0]["max_output_tokens"], 1000);
    close(&tx);
    assert!(read_until(&mut body, &mut buf, |n, _| n == "done").await);
    for bucket in ["total", "tier:premium"] {
        let q = h.quota(USER_1, bucket, "daily").await.unwrap();
        assert_eq!(q.reserved_credits_micro, 0);
        assert_eq!(q.spent_credits_micro, 18, "1*3 + 1*15");
    }

    // A reserve that does not fit is rejected although spent < limit.
    let std_reserve = {
        let c = h.create_chat(U1, Some("standard-1")).await;
        let rid = rid_of(&h.send_message(U1, c, json!({"content": "probe"})).await);
        h.turn(c, rid).await.reserved_credits_micro.unwrap()
    };
    let h2 = Harness::with(Options { standard_limits: (std_reserve + std_reserve / 2, 1_000_000_000), ..Options::default() }).await;
    let c1 = h2.create_chat(U1, Some("standard-1")).await;
    let c2 = h2.create_chat(U1, Some("standard-1")).await;
    let (t1, _d1) = h2.provider.push_channel();
    let (t2, _d2) = h2.provider.push_channel();
    let (p1, p2) = (format!("/chats/{c1}/messages:stream"), format!("/chats/{c2}/messages:stream"));
    let (a, b) = tokio::join!(
        h2.open(U1, "POST", &p1, Some(json!({"content": "probe"}))),
        h2.open(U1, "POST", &p2, Some(json!({"content": "probe"})))
    );
    let mut statuses = [a.0, b.0];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 429], "concurrent reserves cannot exceed the limit");
    let q = h2.quota(USER_1, "total", "daily").await.unwrap();
    assert!(q.reserved_credits_micro <= std_reserve + std_reserve / 2);
    close(&t1);
    close(&t2);
    drop((a, b));
    assert_eq!(h2.provider.chat_requests().len(), 1, "the rejected request never reached the provider");
}

/// Tier downgrade applied when the higher tier is exhausted.
#[tokio::test]
async fn tier_downgrade_when_premium_exhausted() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    h.seed_spent(TENANT_A, USER_1, "tier:premium", "daily", 50_000_000).await;
    let r = h.send_message(U1, chat, json!({"content": "hi"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let done = r.event("done").unwrap();
    assert_eq!(done["selected_model"], "premium-1");
    assert_eq!(done["effective_model"], "standard-1");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_from"], "premium-1");
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");
    assert_eq!(h.provider.chat_requests().pop().unwrap()["model"], "standard-1-provider");
    // Billed at standard multipliers, only in total: 10 + 15 = 25.
    assert_eq!(h.quota(USER_1, "total", "daily").await.unwrap().spent_credits_micro, 25);
    assert_eq!(h.quota(USER_1, "tier:premium", "daily").await.unwrap().spent_credits_micro, 50_000_000);
    // The chat's model is unchanged; the message records the effective model.
    assert_eq!(h.call(U1, "GET", &format!("/chats/{chat}"), None).await.json()["model"], "premium-1");
    let msgs = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json();
    assert_eq!(msgs["items"][1]["model"], "standard-1");
    // Monthly exhaustion also downgrades.
    let hm = Harness::new().await;
    let c = hm.create_chat(U1, None).await;
    hm.seed_spent(TENANT_A, USER_1, "tier:premium", "monthly", 500_000_000).await;
    assert_eq!(hm.send_message(U1, c, json!({"content": "hi"})).await.event("done").unwrap()["effective_model"], "standard-1");
    // Both tiers exhausted -> 429 before any provider call.
    h.seed_spent(TENANT_A, USER_1, "total", "daily", 100_000_000).await;
    let calls = h.provider.chat_requests().len();
    let r = h.send_message(U1, chat, json!({"content": "hi"})).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.resource_exhausted.v1~");
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(h.provider.chat_requests().len(), calls);
    // Kill switches force the standard tier.
    for (switch, reason) in [("force_standard_tier", "force_standard_tier"), ("disable_premium_tier", "disable_premium_tier")] {
        let h = Harness::with(Options { kill_switches: json!({switch: true}), ..Options::default() }).await;
        let chat = h.create_chat(U1, None).await;
        let done = h.send_message(U1, chat, json!({"content": "hi"})).await.event("done").unwrap();
        assert_eq!(done["effective_model"], "standard-1");
        assert_eq!(done["downgrade_reason"], reason);
    }
    // A standard chat is never upgraded and has no downgrade.
    let h = Harness::new().await;
    let chat = h.create_chat(U1, Some("standard-1")).await;
    let done = h.send_message(U1, chat, json!({"content": "hi"})).await.event("done").unwrap();
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_reason").is_none());
}

/// Credits and tokens are accounted correctly per model and tier.
#[tokio::test]
async fn credits_and_tokens_per_model_and_tier() {
    let mut catalog = default_catalog();
    catalog
        .as_array_mut()
        .unwrap()
        .push(model_entry("frac", "standard", 1_500_000, 2_500_000, json!({})));
    let h = Harness::with(Options { catalog, ..Options::default() }).await;
    let premium = h.create_chat(U1, None).await;
    h.provider.push(completed(&["a"], 1000, 200));
    h.send_message(U1, premium, json!({"content": "q"})).await; // 3000 + 3000
    let standard = h.create_chat(U1, Some("standard-1")).await;
    h.provider.push(completed(&["b"], 1000, 200));
    h.send_message(U1, standard, json!({"content": "q"})).await; // 1000 + 600
    let frac = h.create_chat(U1, Some("frac")).await;
    h.provider.push(completed(&["c"], 3, 3));
    h.send_message(U1, frac, json!({"content": "q"})).await; // ceil(4.5) + ceil(7.5) = 5 + 8
    let total = h.quota(USER_1, "total", "daily").await.unwrap();
    assert_eq!(total.spent_credits_micro, 6000 + 1600 + 13);
    assert_eq!(total.input_tokens, 2003);
    assert_eq!(total.output_tokens, 403);
    assert_eq!(total.calls, 3);
    let monthly = h.quota(USER_1, "total", "monthly").await.unwrap();
    assert_eq!(monthly.spent_credits_micro, total.spent_credits_micro);
    let prem = h.quota(USER_1, "tier:premium", "daily").await.unwrap();
    assert_eq!(prem.spent_credits_micro, 6000);
    assert_eq!(prem.calls, 1);
    let published = h.wait_published(3).await;
    let mut credits: Vec<i64> = published.iter().map(|p| p.actual_credits_micro).collect();
    credits.sort_unstable();
    assert_eq!(credits, vec![13, 1600, 6000]);
    let p = published.iter().find(|p| p.actual_credits_micro == 6000).unwrap();
    let usage = p.usage.as_ref().unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (1000, 200));
    assert_eq!(p.effective_model, "premium-1");
    assert_eq!(p.selected_model, "premium-1");
    // Assistant message records tokens.
    let msgs = h.messages(premium).await;
    assert_eq!((msgs[1].input_tokens, msgs[1].output_tokens), (1000, 200));
}

/// Every terminal outcome settles exactly once with actual or estimated usage.
#[tokio::test]
async fn settlement_exactly_once_actual_or_estimated() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, Some("standard-1")).await;
    // completed -> actual
    let r1 = rid_of(&h.send_message(U1, chat, json!({"content": "a"})).await);
    // failed with usage -> actual
    h.provider.push(Script::Sse(vec![(
        "response.failed".into(),
        json!({"response": {"error": {"message": "x"}, "usage": {"input_tokens": 7, "output_tokens": 2}}}),
    )]));
    let r2 = rid_of(&h.send_message(U1, chat, json!({"content": "b"})).await);
    // failed without usage -> estimated
    h.provider.push(Script::Http(500, json!({"error": {"message": "x"}}), vec![]));
    let r3 = rid_of(&h.send_message(U1, chat, json!({"content": "c"})).await);
    // completed without usage -> estimated
    h.provider.push(Script::Sse(vec![
        ("response.output_text.delta".into(), json!({"delta": "no usage"})),
        ("response.completed".into(), json!({"response": {}})),
    ]));
    let r4 = rid_of(&h.send_message(U1, chat, json!({"content": "d"})).await);
    // failed with an all-zero usage object -> usage unknown -> estimated
    h.provider.push(Script::Sse(vec![(
        "response.failed".into(),
        json!({"response": {"error": {"message": "x"}, "usage": {"input_tokens": 0, "output_tokens": 0}}}),
    )]));
    let r6 = rid_of(&h.send_message(U1, chat, json!({"content": "f"})).await);
    // overshoot beyond tolerance -> capped at the reserve
    h.provider.push(completed(&["huge"], 900_000, 100_000));
    let r5 = rid_of(&h.send_message(U1, chat, json!({"content": "e"})).await);

    let published = h.wait_published(6).await;
    assert_eq!(published.len(), 6);
    let ev = |rid: Uuid| published.iter().find(|p| p.request_id == rid).unwrap().clone();
    let (e1, e2, e3, e4, e5) = (ev(r1), ev(r2), ev(r3), ev(r4), ev(r5));
    assert_eq!((e1.terminal_state.as_str(), e1.billing_outcome.as_str(), e1.settlement_method.as_str()), ("completed", "completed", "actual"));
    assert_eq!(e1.actual_credits_micro, 10 + 15);
    assert_eq!((e2.terminal_state.as_str(), e2.billing_outcome.as_str(), e2.settlement_method.as_str()), ("failed", "failed", "actual"));
    assert_eq!(e2.actual_credits_micro, 7 + 6);
    assert_eq!((e3.terminal_state.as_str(), e3.settlement_method.as_str()), ("failed", "estimated"));
    assert!(e3.usage.is_none());
    let t3 = h.turn(chat, r3).await;
    let est_input = t3.reserve_tokens.unwrap() - i64::from(t3.max_output_tokens_applied.unwrap());
    let floor = i64::from(t3.minimal_generation_floor_applied.unwrap());
    assert_eq!(e3.actual_credits_micro, est_input + floor * 3, "estimated = input estimate + generation floor");
    // A completed turn always settles actual (DESIGN 5.8), even without usage.
    assert_eq!(e4.settlement_method, "actual");
    assert_eq!(e4.terminal_state, "completed");
    let e6 = ev(r6);
    assert_eq!((e6.terminal_state.as_str(), e6.settlement_method.as_str()), ("failed", "estimated"));
    assert!(e6.actual_credits_micro > 0, "no free failure");
    let t5 = h.turn(chat, r5).await;
    assert_eq!(e5.settlement_method, "actual");
    assert_eq!(e5.actual_credits_micro, t5.reserved_credits_micro.unwrap(), "overshoot capped at the reserve");
    // Quota spent equals the sum of settled credits; nothing left reserved.
    let q = h.quota(USER_1, "total", "daily").await.unwrap();
    assert_eq!(q.spent_credits_micro, published.iter().map(|p| p.actual_credits_micro).sum::<i64>());
    assert_eq!(q.reserved_credits_micro, 0);
    // No second settlement later.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(h.policy.published.lock().unwrap().len(), 6);
    assert_eq!(h.app.orphan_scan().await.unwrap(), 0);
}

/// Usage is published reliably and exactly once per turn.
#[tokio::test]
async fn usage_published_reliably_exactly_once() {
    let h = Harness::new().await;
    h.policy.fail_publish.store(2, Ordering::SeqCst);
    let chat = h.create_chat(U1, None).await;
    let rid = rid_of(&h.send_message(U1, chat, json!({"content": "a"})).await);
    let mut published = Vec::new();
    for _ in 0..600 {
        published = h.policy.published.lock().unwrap().clone();
        if !published.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(published.len(), 1, "delivered after transient failures");
    assert!(h.policy.publish_attempts.load(Ordering::SeqCst) >= 3);
    let e = &published[0];
    let turn = h.turn(chat, rid).await;
    assert_eq!(e.dedupe_key, format!("{}/{}/{}", TENANT_A.simple(), turn.id.simple(), rid.simple()));
    assert_eq!(e.tenant_id, TENANT_A);
    assert_eq!(e.user_id, Some(USER_1));
    assert_eq!(e.chat_id, chat);
    assert_eq!(e.turn_id, Some(turn.id));
    assert_eq!(e.requester_type, "user");
    assert_eq!(e.policy_version_applied, turn.policy_version_applied.unwrap() as u64);
    // Replay and reads publish nothing more.
    h.send_message(U1, chat, json!({"content": "a", "request_id": rid})).await;
    h.call(U1, "GET", "/quota/status", None).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(h.policy.published.lock().unwrap().len(), 1);
    // One event per turn over many turns.
    for i in 0..4 {
        h.send_message(U1, chat, json!({"content": format!("m{i}")})).await;
    }
    let all = h.wait_published(5).await;
    let mut keys: Vec<&str> = all.iter().map(|p| p.dedupe_key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 5);
    assert_eq!(all.len(), 5);
}

/// Web search tool use is reported, cited, accounted and quota-limited.
#[tokio::test]
async fn web_search_reported_cited_accounted_limited() {
    let h = Harness::with(Options { config: json!({"quota": {"web_search_daily_quota": 2}}), ..Options::default() }).await;
    let chat = h.create_chat(U1, Some("standard-1")).await;
    let web_turn = |n: usize| {
        let mut f = vec![];
        for _ in 0..n {
            f.push(("response.web_search_call.searching".to_owned(), json!({})));
            f.push(("response.web_search_call.completed".to_owned(), json!({})));
        }
        f.push(("response.output_text.delta".to_owned(), json!({"delta": "Found it."})));
        f.push(("response.output_text.annotation.added".to_owned(), json!({"annotation": {"type": "url_citation", "url": "https://example.com/a", "title": "A", "start_index": 0, "end_index": 5}})));
        f.push(("response.completed".to_owned(), json!({"response": {"usage": {"input_tokens": 10, "output_tokens": 5}}})));
        Script::Sse(f)
    };
    // Without web_search: no tool.
    let plain = rid_of(&h.send_message(U1, chat, json!({"content": "x"})).await);
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req["tools"].as_array().is_none_or(|t| t.iter().all(|x| x["type"] != "web_search")));
    assert!(!req["instructions"].as_str().unwrap_or_default().contains("web_search"));

    h.provider.push(web_turn(1));
    let r = h.send_message(U1, chat, json!({"content": "news", "web_search": {"enabled": true}})).await;
    let rid = rid_of(&r);
    let req = h.provider.chat_requests().pop().unwrap();
    let ws = req["tools"].as_array().unwrap().iter().find(|t| t["type"] == "web_search").unwrap().clone();
    assert_eq!(ws["search_context_size"], "low");
    assert_eq!(req["max_tool_calls"], 3);
    assert!(req["instructions"].as_str().unwrap().contains("web_search"), "web search guard");
    let tools: Vec<_> = r.events().into_iter().filter(|(n, _)| n == "tool").map(|(_, d)| d).collect();
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().all(|t| t["name"] == "web_search"));
    assert_eq!(r.event("citations").unwrap()["items"][0]["url"], "https://example.com/a");
    // Surcharge in the reserve.
    let (t_plain, t_web) = (h.turn(chat, plain).await, h.turn(chat, rid).await);
    assert!(t_web.reserve_tokens.unwrap() > t_plain.reserve_tokens.unwrap(), "web search surcharge");
    assert_eq!(t_web.web_search_completed_count, 1);
    assert!(t_web.web_search_enabled);
    // Accounting.
    assert_eq!(h.quota(USER_1, "total", "daily").await.unwrap().web_search_calls, 1);
    let published = h.wait_published(2).await;
    assert_eq!(published.iter().find(|p| p.request_id == rid).unwrap().web_search_calls, 1);

    // Per-message limit (2): the third call fails the turn.
    h.provider.push(web_turn(3));
    let r = h.send_message(U1, chat, json!({"content": "more", "web_search": {"enabled": true}})).await;
    let err = r.event("error").unwrap();
    assert_eq!(err["code"], "web_search_calls_exceeded");
    let t = h.turn(chat, rid_of(&r)).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("web_search_calls_exceeded"));
    // Completed calls before the limit are accounted: 1 + 2 = 3 >= daily quota of 2.
    assert_eq!(h.quota(USER_1, "total", "daily").await.unwrap().web_search_calls, 3);
    let calls = h.provider.chat_requests().len();
    let r = h.send_message(U1, chat, json!({"content": "again", "web_search": {"enabled": true}})).await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(h.provider.chat_requests().len(), calls);
    // Requests without web search still work.
    assert_eq!(h.send_message(U1, chat, json!({"content": "plain"})).await.status, 200);

    // Kill switch.
    let h = Harness::with(Options { kill_switches: json!({"disable_web_search": true}), ..Options::default() }).await;
    let chat = h.create_chat(U1, None).await;
    let r = h.send_message(U1, chat, json!({"content": "x", "web_search": {"enabled": true}})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(r.json()["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "web_search");
    assert!(h.provider.chat_requests().is_empty());
}
