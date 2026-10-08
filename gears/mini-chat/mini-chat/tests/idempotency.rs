//! US3: replay, request-id conflicts, parallel turn guard, turn status, orphan watchdog.
//!
//! AC: Idempotency & Replay, Parallel Turn Enforcement, Turn Lifecycle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use http_body_util::BodyExt;
use serde_json::json;
use uuid::Uuid;

fn reason(p: &serde_json::Value) -> String {
    p["context"]["reason"].as_str().unwrap_or_default().to_owned()
}

/// Wait until the chat has a running turn, then return it.
async fn running_turn(h: &Harness, chat: Uuid) -> ent::chat_turns::Model {
    for _ in 0..200 {
        if let Some(t) = h.turns(chat).await.into_iter().find(|t| t.state == "running") {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no running turn");
}

async fn wait_state(h: &Harness, chat: Uuid, rid: Uuid, state: &str) {
    for _ in 0..200 {
        if h.turns(chat).await.iter().any(|t| t.request_id == rid && t.state == state) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("turn {rid} never reached {state}: {:?}", h.turns(chat).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_of_completed_turn_is_side_effect_free() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let rid = Uuid::new_v4();
    let (_, first) = h.stream(ALICE, chat, json!({"content": "q", "request_id": rid})).await;
    h.eventually("usage", || h.usage_events().len() == 1).await;
    let quota_before: Vec<(String, String, i64, i64)> = h
        .quota_rows(ALICE)
        .await
        .into_iter()
        .map(|r| (r.bucket, r.period_type, r.spent_credits_micro, r.reserved_credits_micro))
        .collect();
    let chat_before = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await.json();

    let (r, ev) = h.stream(ALICE, chat, json!({"content": "different text is ignored", "request_id": rid})).await;
    assert_eq!(r.status, 200);
    assert_eq!(names(&ev), vec!["stream_started", "delta", "done"]);
    let s = find(&ev, "stream_started");
    assert_eq!(s["is_new_turn"], json!(false));
    assert_eq!(s["request_id"], json!(rid.to_string()));
    assert_eq!(s["message_id"], find(&first, "stream_started")["message_id"]);
    assert_eq!(find(&ev, "delta")["content"], json!("Hello from the fake provider"));
    let done = find(&ev, "done");
    assert_eq!(done["usage"], json!({"input_tokens": 42, "output_tokens": 7}));
    assert_eq!(done["quota_decision"], json!("allow"));
    assert!(done.get("downgrade_reason").is_none());

    // No provider call, no quota change, no new rows, no new outbox events.
    assert_eq!(h.provider.chat_requests().len(), 1);
    let quota_after: Vec<(String, String, i64, i64)> = h
        .quota_rows(ALICE)
        .await
        .into_iter()
        .map(|r| (r.bucket, r.period_type, r.spent_credits_micro, r.reserved_credits_micro))
        .collect();
    assert_eq!(quota_before, quota_after);
    assert_eq!(h.turns(chat).await.len(), 1);
    assert_eq!(h.messages(chat).await.len(), 2);
    let chat_after = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await.json();
    assert_eq!(chat_before["updated_at"], chat_after["updated_at"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.usage_events().len(), 1);
    assert_eq!(h.audit_events().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_id_conflicts_for_failed_cancelled_running_and_deleted() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;

    // failed
    let failed = Uuid::new_v4();
    h.provider.push(Reply::Status { status: 500, body: json!({"error": {"message": "x"}}), retry_after: None });
    h.stream(ALICE, chat, json!({"content": "a", "request_id": failed})).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "a", "request_id": failed}))).await;
    assert_eq!(reason(&r.problem(409)), "request_id_conflict");
    assert!(r.headers["content-type"].to_str().unwrap().contains("json"), "no SSE stream opened");

    // cancelled
    let cancelled = Uuid::new_v4();
    h.provider.push(Reply::slow("long answer", Duration::from_millis(300)));
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "b", "request_id": cancelled})).await;
    let mut body = resp.into_body();
    body.frame().await;
    drop(body);
    wait_state(&h, chat, cancelled, "cancelled").await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "b", "request_id": cancelled}))).await;
    assert_eq!(reason(&r.problem(409)), "request_id_conflict");

    // running: the same request id → request_id_conflict (idempotency is checked first)
    let running = Uuid::new_v4();
    h.provider.push(Reply::slow("slow answer here", Duration::from_millis(400)));
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "c", "request_id": running})).await;
    running_turn(&h, chat).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "c", "request_id": running}))).await;
    assert_eq!(reason(&r.problem(409)), "request_id_conflict");
    // a different request id while running → turn_already_running
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "d", "request_id": Uuid::new_v4()}))).await;
    assert_eq!(reason(&r.problem(409)), "turn_already_running");
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "d"}))).await;
    assert_eq!(reason(&r.problem(409)), "turn_already_running");
    let all = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&all).contains("event: done"));

    // A new turn is accepted once the previous one is terminal.
    let ev = h.say(ALICE, chat, "after").await;
    assert_eq!(names(&ev).last(), Some(&"done"));

    // deleted (latest turn deleted) → conflict
    let last = h.turns(chat).await.last().unwrap().request_id;
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}/turns/{last}"), None).await;
    assert_eq!(r.status, 204, "{}", r.text());
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x", "request_id": last}))).await;
    assert_eq!(reason(&r.problem(409)), "request_id_conflict");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_is_checked_before_parallel_guard() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let done_rid = Uuid::new_v4();
    h.stream(ALICE, chat, json!({"content": "first", "request_id": done_rid})).await;
    h.provider.push(Reply::slow("busy busy busy", Duration::from_millis(300)));
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "second"})).await;
    running_turn(&h, chat).await;
    // Replay of the completed turn succeeds although another turn is running.
    let (r, ev) = h.stream(ALICE, chat, json!({"content": "first", "request_id": done_rid})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(find(&ev, "stream_started")["is_new_turn"], json!(false));
    drop(resp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_one_turn_runs_per_chat_under_concurrency() {
    let h = Arc::new(Harness::new().await);
    let chat = h.chat(ALICE, Some("standard-m")).await;
    for _ in 0..6 {
        h.provider.push(Reply::slow("a b c", Duration::from_millis(100)));
    }
    let mut tasks = Vec::new();
    for i in 0..6 {
        let h = Arc::clone(&h);
        tasks.push(tokio::spawn(async move {
            h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": format!("m{i}")}))).await
        }));
    }
    let mut ok = 0;
    let mut conflicts = 0;
    for t in tasks {
        let r = t.await.unwrap();
        match r.status {
            200 => ok += 1,
            409 => {
                assert_eq!(reason(&r.json()), "turn_already_running");
                conflicts += 1;
            }
            s => panic!("unexpected {s}: {}", r.text()),
        }
    }
    assert!(ok >= 1);
    assert_eq!(ok + conflicts, 6);
    let turns = h.turns(chat).await;
    assert_eq!(turns.len(), ok, "rejected requests leave no turn rows");
    // Never two running turns: completed turns don't overlap in time.
    let mut spans: Vec<_> = turns.iter().map(|t| (t.started_at, t.completed_at.unwrap())).collect();
    spans.sort();
    for w in spans.windows(2) {
        assert!(w[0].1 <= w[1].0, "overlapping turns {w:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_status_in_every_state() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    // completed
    let ok = Uuid::new_v4();
    h.stream(ALICE, chat, json!({"content": "a", "request_id": ok})).await;
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/turns/{ok}"), None).await;
    assert_eq!(r.status, 200);
    let s = r.json();
    assert_eq!(s["state"], json!("done"));
    assert_eq!(s["request_id"], json!(ok.to_string()));
    assert!(s["assistant_message_id"].is_string());
    assert!(s.get("error_code").is_none());
    assert!(s["updated_at"].is_string());

    // failed
    let bad = Uuid::new_v4();
    h.provider.push(Reply::Status { status: 500, body: json!({}), retry_after: None });
    h.stream(ALICE, chat, json!({"content": "b", "request_id": bad})).await;
    let s = h.send(ALICE, "GET", &format!("/chats/{chat}/turns/{bad}"), None).await.json();
    assert_eq!(s["state"], json!("error"));
    assert_eq!(s["error_code"], json!("provider_error"));
    assert!(s.get("assistant_message_id").is_none());

    // cancelled
    let can = Uuid::new_v4();
    h.provider.push(Reply::slow("x y z", Duration::from_millis(300)));
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "c", "request_id": can})).await;
    // running
    running_turn(&h, chat).await;
    let s = h.send(ALICE, "GET", &format!("/chats/{chat}/turns/{can}"), None).await.json();
    assert_eq!(s["state"], json!("running"));
    drop(resp);
    wait_state(&h, chat, can, "cancelled").await;
    let s = h.send(ALICE, "GET", &format!("/chats/{chat}/turns/{can}"), None).await.json();
    assert_eq!(s["state"], json!("cancelled"));

    // unknown, foreign, deleted → 404
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/turns/{}", Uuid::new_v4()), None).await;
    r.problem(404);
    let r = h.send(BOB, "GET", &format!("/chats/{chat}/turns/{ok}"), None).await;
    r.problem(404);
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}/turns/{can}"), None).await;
    assert_eq!(r.status, 204, "{}", r.text());
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/turns/{can}"), None).await;
    r.problem(404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphan_watchdog_finalizes_stale_running_turn() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::slow("never finishes in time", Duration::from_secs(2)));
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "x"})).await;
    let t = running_turn(&h, chat).await;
    // A fresh running turn is not touched.
    assert_eq!(h.svc.orphan_scan().await.unwrap(), 0);
    h.age_turn(t.id, 3600).await;
    assert_eq!(h.svc.orphan_scan().await.unwrap(), 1);
    let after = h.turns(chat).await.into_iter().find(|x| x.id == t.id).unwrap();
    assert_eq!(after.state, "failed");
    assert_eq!(after.error_code.as_deref(), Some("orphan_timeout"));
    let row = h.quota_row(ALICE, "total", "daily").await.unwrap();
    assert_eq!(row.reserved_credits_micro, 0, "reserve released");
    h.eventually("usage", || h.usage_events().len() == 1).await;
    let u = &h.usage_events()[0];
    assert_eq!(u.billing_outcome, "aborted");
    assert_eq!(u.settlement_method, "estimated");
    assert_eq!(row.spent_credits_micro, u.actual_credits_micro);
    h.eventually("audit", || !h.audit_events().is_empty()).await;
    assert_eq!(h.audit_events()[0].event_type(), "turn_failed");
    // Second scan is a no-op (CAS): settled exactly once.
    assert_eq!(h.svc.orphan_scan().await.unwrap(), 0);
    drop(resp);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.usage_events().len(), 1);
    let final_turn = h.turns(chat).await.into_iter().find(|x| x.id == t.id).unwrap();
    assert_eq!(final_turn.state, "failed", "late relay task cannot overwrite the orphan finalization");
    // The chat accepts a new turn.
    h.say(ALICE, chat, "again").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphan_watchdog_worker_loop_runs_as_leader() {
    let mut opts = Opts::default();
    opts.cfg.orphan_watchdog.scan_interval_secs = 1;
    let h = Harness::with(opts).await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::slow("slow", Duration::from_secs(3)));
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "x"})).await;
    let t = running_turn(&h, chat).await;
    h.age_turn(t.id, 3600).await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let handle = mini_chat::infra::workers::spawn_watchdog(Arc::clone(&h.svc), Arc::new(mini_chat::infra::workers::NoopElector), cancel.clone());
    wait_state(&h, chat, t.request_id, "failed").await;
    cancel.cancel();
    handle.await.unwrap();
    drop(resp);
}
