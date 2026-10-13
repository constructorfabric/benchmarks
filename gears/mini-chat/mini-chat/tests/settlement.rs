//! T060: settlement exactly once per terminal outcome; usage published once; orphan watchdog.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use mini_chat_sdk::AuditEvent;
use serde_json::json;
use tokio::sync::Notify;
use uuid::Uuid;

/// Reserve for content "x" on `prem`: 13 estimated input tokens, 1000 max output tokens.
const RESERVE: i64 = 13 + 2 * 1000;
/// Estimated charge: estimated input + minimal generation floor (50) output tokens.
const ESTIMATED: i64 = 13 + 2 * 50;

async fn settle_check(h: &Harness, spent: i64, calls: i32) {
    let rows = db::quota_rows(h).await;
    for r in &rows {
        assert_eq!(r.reserved_credits_micro, 0, "reserve released: {r:?}");
        assert_eq!(r.spent_credits_micro, spent, "{r:?}");
        assert_eq!(r.calls, calls, "{r:?}");
    }
    assert_eq!(rows.len(), 4, "total+premium x daily+monthly");
}

#[tokio::test]
async fn completed_turn_settles_actual() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["a".into()],
        usage: (10, 7),
        before_done: vec![],
        output: None,
    });
    let rid = Uuid::new_v4();
    h.send_body(chat, json!({"content": "x", "request_id": rid}))
        .await;
    settle_check(&h, 10 + 14, 1).await;
    let t = db::turns(&h, chat).await;
    assert_eq!(t[0].reserved_credits_micro, Some(RESERVE));
    assert_eq!(t[0].effective_model.as_deref(), Some("prem"));
    assert_eq!(t[0].policy_version_applied, Some(1));
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.published_for(rid).len(),
        1,
        "usage published exactly once"
    );
    let ev = &h.published_for(rid)[0];
    let u = ev.usage.as_ref().unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (10, 7));
    assert_eq!(ev.selected_model, "prem");
    assert_eq!(ev.effective_model, "prem");
    assert_eq!(ev.policy_version_applied, 1);
    assert_eq!(ev.requester_type, "user");
    assert_eq!(ev.user_id, Some(h.user));
    assert_eq!(ev.chat_id, Some(chat));
    // zero usage on a completed turn costs nothing
    h.provider.push(Script::Ok {
        parts: vec![],
        usage: (0, 0),
        before_done: vec![],
        output: None,
    });
    h.send(chat, "x").await;
    settle_check(&h, 24, 2).await;
}

#[tokio::test]
async fn failed_and_cancelled_turns_settle_estimated() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Failed {
        parts: vec![],
        message: "x".into(),
        usage: None,
    });
    h.send(chat, "x").await;
    settle_check(&h, ESTIMATED, 1).await;

    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["p".into()],
        gate,
        usage: (1, 1),
    });
    let rid = Uuid::new_v4();
    let mut s = h
        .open_send(chat, json!({"content": "x", "request_id": rid}))
        .await;
    assert!(s.until("delta").await);
    drop(s);
    h.wait_turn_terminal(chat).await;
    // prior context now adds tokens to the estimate, so only check monotonic + released
    let rows = db::quota_rows(&h).await;
    for r in &rows {
        assert_eq!(r.reserved_credits_micro, 0);
        assert_eq!(r.calls, 2);
        assert!(r.spent_credits_micro > ESTIMATED);
    }
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    assert_eq!(h.published_for(rid)[0].settlement_method, "estimated");
}

#[tokio::test]
async fn overshoot_is_capped_at_reserve() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["a".into()],
        usage: (5000, 5000),
        before_done: vec![],
        output: None,
    });
    let r = h.send(chat, "x").await;
    assert_eq!(r.names().last(), Some(&"done"), "turn stays completed");
    settle_check(&h, RESERVE, 1).await;
    // within tolerance (<= 1.10 x reserve tokens) the actual is committed
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["a".into()],
        usage: (100, 1000),
        before_done: vec![],
        output: None,
    });
    h.send(chat, "x").await;
    settle_check(&h, 100 + 2000, 1).await;
}

#[tokio::test]
async fn orphan_watchdog_finalizes_stale_turns_once() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["p".into()],
        gate: gate.clone(),
        usage: (1, 1),
    });
    let rid = Uuid::new_v4();
    let mut s = h
        .open_send(chat, json!({"content": "x", "request_id": rid}))
        .await;
    assert!(s.until("delta").await);
    // fresh turns are not touched
    assert_eq!(h.core.scan_orphans().await, 0);
    db::set_turn_started_back(&h, rid, 1000).await;
    assert_eq!(h.core.scan_orphans().await, 1);
    let t = db::turns(&h, chat).await;
    assert_eq!(t[0].state, "failed");
    assert_eq!(t[0].error_code.as_deref(), Some("orphan_timeout"));
    settle_check(&h, ESTIMATED, 1).await;
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    let ev = &h.published_for(rid)[0];
    assert_eq!(ev.billing_outcome, "aborted");
    assert_eq!(ev.settlement_method, "estimated");
    assert_eq!(ev.actual_credits_micro, ESTIMATED);
    assert!(h.eventually(|| h.audit_events().iter().any(|e| matches!(e, AuditEvent::Turn(t) if t.event_type == "turn_failed" && t.request_id == rid))).await);

    // the provider finishing later loses the CAS: no second settlement / event
    gate.notify_one();
    s.until("done").await;
    assert!(!s.names().contains(&"done"), "{:?}", s.names());
    assert_eq!(s.names().last(), Some(&"error"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    settle_check(&h, ESTIMATED, 1).await;
    assert_eq!(h.published_for(rid).len(), 1);
    assert_eq!(h.core.scan_orphans().await, 0);
    let (_, v) = h
        .get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"))
        .await;
    assert_eq!(v["state"], "error");
    assert_eq!(v["error_code"], "orphan_timeout");
}
