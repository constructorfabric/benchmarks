//! Background processing: orphan watchdog, usage publication (exactly once
//! per turn) and thread summaries (acceptance criteria: Cleanup & Recovery,
//! Settlement & Finalization, Thread summary; DESIGN §3.6 "Thread Summary
//! Update", §3.9 "Summary Interaction on Turn Mutation", §4 "Orphan Turn
//! Watchdog").

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use mini_chat::domain::service::stream::{SendRequest, StreamEvent, StreamStart};
use mini_chat_sdk::UsageEvent;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

// ── local helpers ────────────────────────────────────────────────────────

fn send_req(content: &str, request_id: Uuid) -> SendRequest {
    SendRequest {
        content: content.to_owned(),
        request_id: Some(request_id),
        attachment_ids: Vec::new(),
        web_search: false,
    }
}

fn events_for(h: &Harness, request_id: Uuid) -> Vec<UsageEvent> {
    h.policy
        .published
        .lock()
        .iter()
        .filter(|e| e.request_id == request_id)
        .cloned()
        .collect()
}

async fn wait_events(h: &Harness, request_id: Uuid) {
    h.eventually(|| async { !events_for(h, request_id).is_empty() }).await;
}

async fn turn_col(h: &Harness, request_id: Uuid, col: &str) -> Option<String> {
    h.scalar_str(&format!(
        "SELECT {col} FROM chat_turns WHERE hex(request_id) = '{}'",
        hex_uuid(request_id)
    ))
    .await
}

/// `(reserved, spent)` of the total daily bucket of a user.
async fn daily_total(h: &Harness, user_id: Uuid) -> (i64, i64) {
    let rows = h
        .query(&format!(
            "SELECT reserved_credits_micro, spent_credits_micro FROM quota_usage \
             WHERE hex(user_id) = '{}' AND period_type = 'daily' AND bucket = 'total'",
            hex_uuid(user_id)
        ))
        .await;
    assert_eq!(rows.len(), 1, "one daily total bucket row");
    (
        rows[0].try_get_by_index::<i64>(0).unwrap(),
        rows[0].try_get_by_index::<i64>(1).unwrap(),
    )
}

/// Full snapshot of a user's quota rows (for "nothing changed" checks).
async fn quota_snapshot(h: &Harness, user_id: Uuid) -> Option<String> {
    h.scalar_str(&format!(
        "SELECT group_concat(period_type || ':' || bucket || ':' || spent_credits_micro || ':' || \
         reserved_credits_micro || ':' || calls || ':' || input_tokens || ':' || output_tokens, '|') \
         FROM (SELECT * FROM quota_usage WHERE hex(user_id) = '{}' ORDER BY period_type, bucket)",
        hex_uuid(user_id)
    ))
    .await
}

async fn start_hanging_turn(
    h: &Harness,
    c: &SecurityContext,
    chat: Uuid,
    request_id: Uuid,
) -> (tokio::sync::mpsc::Receiver<StreamEvent>, tokio_util::sync::CancellationToken) {
    h.provider.push(Reply::Hang(vec![delta("partial")]));
    let start = h.svc.send_message(c, chat, send_req("hang please", request_id)).await.unwrap();
    let StreamStart::Live { mut events, cancel } = start else {
        panic!("expected a live stream");
    };
    let first = next_n(&mut events, 2).await;
    assert_eq!(names(&first), vec!["stream_started", "delta"]);
    (events, cancel)
}

fn summary_calls(h: &Harness) -> usize {
    h.provider
        .chat_requests()
        .iter()
        .filter(|b| b.get("stream") == Some(&Value::Bool(false)))
        .count()
}

fn streaming_requests(h: &Harness) -> Vec<Value> {
    h.provider
        .chat_requests()
        .into_iter()
        .filter(|b| b.get("stream") == Some(&Value::Bool(true)))
        .collect()
}

async fn summary_rows(h: &Harness, chat: Uuid) -> i64 {
    h.scalar_i64(&format!(
        "SELECT count(*) FROM thread_summaries WHERE hex(chat_id) = '{}'",
        hex_uuid(chat)
    ))
    .await
}

async fn compressed(h: &Harness, chat: Uuid) -> i64 {
    h.scalar_i64(&format!(
        "SELECT count(*) FROM messages WHERE hex(chat_id) = '{}' AND is_compressed = 1",
        hex_uuid(chat)
    ))
    .await
}

fn long_text(i: usize) -> String {
    format!("message {i}: {}", "lorem ipsum dolor sit amet ".repeat(16))
}

fn rid_of(r: &HttpResp) -> Uuid {
    Uuid::parse_str(r.sse()[0].1["request_id"].as_str().unwrap()).unwrap()
}

/// Sends long messages to a `tiny` chat until the summary trigger fires
/// (a non-streaming summary request reaches the provider). Returns the
/// request ids of the sent turns.
async fn send_until_summary_call(h: &Harness, c: &SecurityContext, chat: Uuid) -> Vec<Uuid> {
    let mut rids = Vec::new();
    for i in 0..15 {
        let r = h.send(c, chat, json!({"content": long_text(i)})).await;
        assert_eq!(r.status, 200, "{}", r.text);
        assert_eq!(r.sse().last().unwrap().0, "done", "{}", r.text);
        rids.push(rid_of(&r));
        for _ in 0..20 {
            if summary_calls(h) > 0 {
                return rids;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    panic!("thread summary never triggered");
}

fn summary_harness_cfg() -> mini_chat::config::MiniChatConfig {
    let mut cfg = base_config();
    cfg.thread_summary_worker.enabled = true;
    cfg.thread_summary_worker.summary_model_id = "std".into();
    cfg
}

// ── orphan watchdog ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn orphan_watchdog_finalizes_stale_turns_with_estimated_charge() {
    let h = Harness::new().await;
    let (uid, tid) = (Uuid::new_v4(), Uuid::new_v4());
    let u = ctx(uid, tid);
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let rid = Uuid::new_v4();
    let (_events, cancel) = start_hanging_turn(&h, &u, chat, rid).await;

    // a second, recently progressing turn of another user
    let (uid2, tid2) = (Uuid::new_v4(), Uuid::new_v4());
    let u2 = ctx(uid2, tid2);
    let chat2 = h.create_chat(&u2, json!({"model": "std"})).await;
    let rid2 = Uuid::new_v4();
    let (_events2, cancel2) = start_hanging_turn(&h, &u2, chat2, rid2).await;

    let (reserved, spent) = daily_total(&h, uid).await;
    assert!(reserved > 0 && spent == 0, "reserve held while running: {reserved}/{spent}");

    // Only last_progress_at is aged: started_at keeps the reserve's period
    // (the watchdog derives the settlement period rows from started_at).
    h.exec(&format!(
        "UPDATE chat_turns SET last_progress_at = '2000-01-01 00:00:00+00:00' WHERE hex(request_id) = '{}'",
        hex_uuid(rid)
    ))
    .await;
    // A long-running turn with an old started_at but recent progress is not an orphan.
    h.exec(&format!(
        "UPDATE chat_turns SET started_at = '2000-01-01 00:00:00+00:00' WHERE hex(request_id) = '{}'",
        hex_uuid(rid2)
    ))
    .await;

    let n = mini_chat::infra::workers::orphan_scan(&h.svc).await.unwrap();
    assert_eq!(n, 1, "only the stale turn is finalized");
    assert_eq!(turn_col(&h, rid, "state").await.as_deref(), Some("failed"));
    assert_eq!(turn_col(&h, rid, "error_code").await.as_deref(), Some("orphan_timeout"));
    assert!(turn_col(&h, rid, "completed_at").await.is_some());
    assert_eq!(turn_col(&h, rid2, "state").await.as_deref(), Some("running"), "progressing turn untouched");

    let (reserved, spent) = daily_total(&h, uid).await;
    assert_eq!(reserved, 0, "reserve released");
    assert!(spent > 0, "estimated charge committed");
    let (reserved2, _) = daily_total(&h, uid2).await;
    assert!(reserved2 > 0, "other user's reserve still held");

    wait_events(&h, rid).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let evs = events_for(&h, rid);
    assert_eq!(evs.len(), 1, "{evs:?}");
    let e = &evs[0];
    assert_eq!(e.billing_outcome, "aborted");
    assert_eq!(e.settlement_method, "estimated");
    assert_eq!(e.terminal_state, "failed");
    assert!(e.usage.is_none());
    assert_eq!(e.actual_credits_micro, spent);
    assert_eq!(e.user_id, Some(uid));
    assert_eq!(e.chat_id, chat);
    // audit: turn_failed with orphan_timeout
    h.eventually(|| async {
        h.audit
            .turns
            .lock()
            .iter()
            .any(|a| a.request_id == rid && a.error_code.as_deref() == Some("orphan_timeout") && a.event_type == "turn_failed")
    })
    .await;

    // the original stream task loses the CAS: no second event, no extra charge
    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(events_for(&h, rid).len(), 1);
    assert_eq!(daily_total(&h, uid).await, (0, spent));
    assert_eq!(turn_col(&h, rid, "error_code").await.as_deref(), Some("orphan_timeout"));

    // a repeated scan finds nothing
    assert_eq!(mini_chat::infra::workers::orphan_scan(&h.svc).await.unwrap(), 0);
    cancel2.cancel();
    h.shutdown().await;
}

// ── usage publication ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn usage_event_published_exactly_once_per_turn() {
    let h = Harness::new().await;
    let (uid, tid) = (Uuid::new_v4(), Uuid::new_v4());
    let u = ctx(uid, tid);
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let rid = Uuid::new_v4();
    let r = h.send(&u, chat, json!({"content": "hi", "request_id": rid})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.sse().last().unwrap().0, "done");
    wait_events(&h, rid).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let evs = events_for(&h, rid);
    assert_eq!(evs.len(), 1, "{evs:?}");
    let e = &evs[0];
    let turn_hex = turn_col(&h, rid, "lower(hex(id))").await.unwrap();
    assert_eq!(
        e.dedupe_key,
        format!("{}/{}/{}", tid.as_simple(), turn_hex, rid.as_simple())
    );
    assert_eq!(e.turn_id.map(|t| t.as_simple().to_string()), Some(turn_hex));
    assert_eq!(e.settlement_method, "actual");
    assert_eq!(e.actual_credits_micro, 250);
    assert_eq!(e.terminal_state, "completed");
    assert_eq!(e.billing_outcome, "completed");
    assert_eq!(e.requester_type, "user");
    assert_eq!(e.effective_model, "std");
    let usage = e.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (100, 50));
    let snap = quota_snapshot(&h, uid).await;
    assert_eq!(daily_total(&h, uid).await, (0, 250));

    // replay of the same request_id: no new event, no quota change
    let r = h.send(&u, chat, json!({"content": "hi", "request_id": rid})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.sse()[0].1["is_new_turn"], false);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(events_for(&h, rid).len(), 1);
    assert_eq!(h.policy.published.lock().len(), 1);
    assert_eq!(quota_snapshot(&h, uid).await, snap);
    h.shutdown().await;
}

// ── thread summary ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn thread_summary_is_created_applied_and_invalidated() {
    let h = Harness::with(summary_harness_cfg(), policy_cfg(default_catalog())).await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "tiny"})).await;
    let rids = send_until_summary_call(&h, &u, chat).await;
    let trigger = *rids.last().unwrap();
    assert!(rids.len() >= 2, "the first turn cannot be summarized");

    h.eventually(|| async { summary_rows(&h, chat).await == 1 }).await;
    let text = h
        .scalar_str(&format!(
            "SELECT summary_text FROM thread_summaries WHERE hex(chat_id) = '{}'",
            hex_uuid(chat)
        ))
        .await;
    assert_eq!(text.as_deref(), Some("The summary"));

    // the summary request went to the summary model, non-streaming
    let sreq = h
        .provider
        .chat_requests()
        .into_iter()
        .find(|b| b.get("stream") == Some(&Value::Bool(false)))
        .unwrap();
    assert_eq!(sreq["model"], "std-provider");
    assert!(
        sreq["input"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with(mini_chat::domain::service::summary::OPENING_NEW),
        "{sreq}"
    );

    // the frontier is the last message before the trigger turn: earlier
    // messages are compressed, the trigger turn's messages are not
    let frontier_rid = h
        .scalar_str(&format!(
            "SELECT lower(hex(m.request_id)) FROM thread_summaries s JOIN messages m \
             ON m.id = s.summarized_up_to_message_id WHERE hex(s.chat_id) = '{}'",
            hex_uuid(chat)
        ))
        .await
        .unwrap();
    assert_eq!(frontier_rid, rids[rids.len() - 2].as_simple().to_string());
    let trigger_compressed = h
        .scalar_i64(&format!(
            "SELECT count(*) FROM messages WHERE hex(request_id) = '{}' AND is_compressed = 1",
            hex_uuid(trigger)
        ))
        .await;
    assert_eq!(trigger_compressed, 0, "the trigger turn is never summarized");
    let expected_compressed = 2 * (i64::try_from(rids.len()).unwrap() - 1);
    assert_eq!(compressed(&h, chat).await, expected_compressed);

    // system-task usage event
    h.eventually(|| async { h.policy.published.lock().iter().any(|e| e.billing_outcome == "system_task") })
        .await;
    let sys: Vec<UsageEvent> = h
        .policy
        .published
        .lock()
        .iter()
        .filter(|e| e.billing_outcome == "system_task")
        .cloned()
        .collect();
    assert_eq!(sys.len(), 1, "{sys:?}");
    assert_eq!(sys[0].requester_type, "system");
    assert_eq!(sys[0].settlement_method, "none");
    assert_eq!(sys[0].chat_id, chat);
    assert!(sys[0].turn_id.is_none() && sys[0].user_id.is_none());
    assert_eq!(sys[0].system_task_type.as_deref(), Some("thread_summary_update"));

    // the next turn applies the summary
    let r = h.send(&u, chat, json!({"content": "and now?"})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let started = &r.sse()[0];
    assert_eq!(started.0, "stream_started");
    assert!(
        started.1["thread_summary_applied"]["token_estimate"].as_i64().unwrap() > 0,
        "{}",
        started.1
    );
    let next = rid_of(&r);
    let req = streaming_requests(&h).last().cloned().unwrap();
    let first = &req["input"][0];
    assert_eq!(first["role"], "user");
    let content = first["content"].as_str().unwrap();
    assert!(
        content.starts_with("This conversation has earlier messages that have been summarized."),
        "{content}"
    );
    assert!(content.contains("The summary"));
    // compressed history is not resent
    let all_input = req["input"].to_string();
    assert!(!all_input.contains("message 0:"), "compressed messages leak into context");
    assert!(all_input.contains(&format!("message {}:", rids.len() - 1)), "trigger turn stays in context");

    // ── mutation-driven invalidation (DESIGN §3.9) ──
    // Retry of the latest turn, whose user message is after the frontier: kept.
    let r = h
        .call(&u, "POST", &format!("/mini-chat/v1/chats/{chat}/turns/{next}/retry"), None)
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.sse().last().unwrap().0, "done", "{}", r.text);
    let retried = rid_of(&r);
    assert_eq!(summary_rows(&h, chat).await, 1, "summary not covering the retried turn is kept");
    assert_eq!(compressed(&h, chat).await, expected_compressed);

    // Delete the replacement and the trigger turn (both after the frontier): kept.
    for rid in [retried, trigger] {
        let d = h
            .call(&u, "DELETE", &format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), None)
            .await;
        assert_eq!(d.status, 204, "{}", d.text);
        assert_eq!(summary_rows(&h, chat).await, 1);
    }

    // Now the latest turn is covered by the summary: retrying it drops the
    // summary and clears is_compressed. Block a new summary from committing.
    *h.provider.summary_text.lock() = String::new();
    let covered = rids[rids.len() - 2];
    let r = h
        .call(&u, "POST", &format!("/mini-chat/v1/chats/{chat}/turns/{covered}/retry"), None)
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(summary_rows(&h, chat).await, 0, "covering summary deleted");
    assert_eq!(compressed(&h, chat).await, 0, "is_compressed cleared");
    let req = streaming_requests(&h).last().cloned().unwrap();
    assert!(
        !req["input"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .starts_with("This conversation has earlier messages"),
        "no summary after invalidation"
    );
    assert!(r.sse()[0].1.get("thread_summary_applied").is_none_or(Value::is_null), "{}", r.sse()[0].1);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_summary_creates_no_thread_summary() {
    let h = Harness::with(summary_harness_cfg(), policy_cfg(default_catalog())).await;
    *h.provider.summary_text.lock() = String::new();
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "tiny"})).await;
    send_until_summary_call(&h, &u, chat).await;
    // the task is retried up to thread_summary_worker.max_attempts (3)
    for _ in 0..200 {
        if summary_calls(&h) >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(summary_calls(&h) >= 2, "empty summary is retried");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(summary_rows(&h, chat).await, 0);
    assert_eq!(compressed(&h, chat).await, 0);
    assert!(
        !h.policy.published.lock().iter().any(|e| e.billing_outcome == "system_task"),
        "no system-task usage without a commit"
    );
    // the next turn has no summary applied
    let r = h.send(&u, chat, json!({"content": "next"})).await;
    assert_eq!(r.status, 200);
    assert!(r.sse()[0].1.get("thread_summary_applied").is_none_or(Value::is_null));
    h.shutdown().await;
}
