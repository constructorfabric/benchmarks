//! Idempotency & replay, and parallel turn enforcement.

use std::sync::atomic::Ordering;

use serde_json::json;
use uuid::Uuid;

use crate::common::*;

/// Replaying a known request id returns the stored result without side effects.
#[tokio::test]
async fn replay_returns_stored_result_without_side_effects() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let rid = Uuid::new_v4();
    h.provider.push(completed(&["Stored", " answer"], 30, 4));
    let first = h.send_message(U1, chat, json!({"content": "q", "request_id": rid})).await;
    assert_eq!(first.status, 200);
    h.wait_published(1).await;
    let calls = h.provider.chat_requests().len();
    let quota_before = h.quota(USER_1, "total", "daily").await.unwrap();
    let msgs_before = h.messages(chat).await.len();

    let replay = h.send_message(U1, chat, json!({"content": "different content ignored", "request_id": rid})).await;
    assert_eq!(replay.status, 200);
    let started = replay.event("stream_started").unwrap();
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["is_new_turn"], false);
    assert_eq!(started["message_id"], first.event("stream_started").unwrap()["message_id"]);
    let text: String = replay.events().iter().filter(|(n, _)| n == "delta").map(|(_, d)| d["content"].as_str().unwrap().to_owned()).collect();
    assert_eq!(text, "Stored answer");
    let done = replay.event("done").unwrap();
    assert_eq!(done["usage"], json!({"input_tokens": 30, "output_tokens": 4}));
    assert_eq!(replay.event_names().last().unwrap(), "done");

    assert_eq!(h.provider.chat_requests().len(), calls, "no provider call on replay");
    let quota_after = h.quota(USER_1, "total", "daily").await.unwrap();
    assert_eq!(quota_after.spent_credits_micro, quota_before.spent_credits_micro);
    assert_eq!(quota_after.reserved_credits_micro, 0);
    assert_eq!(quota_after.calls, quota_before.calls);
    assert_eq!(h.messages(chat).await.len(), msgs_before);
    assert_eq!(h.turns(chat).await.len(), 1);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(h.policy.published.lock().unwrap().len(), 1, "no usage event on replay");
}

/// Conflicting reuse of a request id is rejected consistently across turn states.
#[tokio::test]
async fn conflicting_request_id_reuse_rejected() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    // Failed turn.
    let failed = Uuid::new_v4();
    h.provider.push(Script::Http(500, json!({"error": {"message": "boom"}}), vec![]));
    h.send_message(U1, chat, json!({"content": "a", "request_id": failed})).await;
    assert_eq!(h.turn(chat, failed).await.state, "failed");
    let r = h.send_message(U1, chat, json!({"content": "a", "request_id": failed})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.reason(), "request_id_conflict");

    // Cancelled turn.
    let cancelled = Uuid::new_v4();
    let (tx, _d) = h.provider.push_channel();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "b", "request_id": cancelled}))).await;
    tx.send(frame("response.output_text.delta", &json!({"delta": "x"})).into()).unwrap();
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "delta").await);
    drop(body);
    let h_ref = &h;
    for _ in 0..200 {
        if h_ref.turn(chat, cancelled).await.state != "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(h.turn(chat, cancelled).await.state, "cancelled");
    let r = h.send_message(U1, chat, json!({"content": "b", "request_id": cancelled})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.reason(), "request_id_conflict");

    // Running turn: the same id conflicts, a different id hits the parallel guard.
    let running = Uuid::new_v4();
    let (tx, _d) = h.provider.push_channel();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "c", "request_id": running}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    let r = h.send_message(U1, chat, json!({"content": "c", "request_id": running})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.reason(), "request_id_conflict");
    tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into()).unwrap();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "done").await);

    // Deleted completed turn: reuse conflicts, does not replay.
    let deleted = Uuid::new_v4();
    h.send_message(U1, chat, json!({"content": "d", "request_id": deleted})).await;
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}/turns/{deleted}"), None).await.status, 204);
    let r = h.send_message(U1, chat, json!({"content": "d", "request_id": deleted})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.reason(), "request_id_conflict");
    // Request ids are scoped per chat.
    let chat2 = h.create_chat(U1, None).await;
    assert_eq!(h.send_message(U1, chat2, json!({"content": "e", "request_id": failed})).await.status, 200);
    let _ = Ordering::SeqCst;
}

/// Replay is checked before the parallel-turn guard.
#[tokio::test]
async fn replay_checked_before_parallel_guard() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let done_rid = Uuid::new_v4();
    h.send_message(U1, chat, json!({"content": "first", "request_id": done_rid})).await;
    let (tx, _d) = h.provider.push_channel();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "second"}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    // A turn is running, yet replaying the completed one succeeds.
    let r = h.send_message(U1, chat, json!({"content": "first", "request_id": done_rid})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.event("stream_started").unwrap()["is_new_turn"], false);
    tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into()).unwrap();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "done").await);
}

/// Only one turn may run per chat; a new one is accepted once the previous is terminal.
#[tokio::test]
async fn one_running_turn_per_chat() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let other = h.create_chat(U1, None).await;
    let (tx, _d) = h.provider.push_channel();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "long"}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    let r = h.send_message(U1, chat, json!({"content": "parallel"})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.reason(), "turn_already_running");
    assert_eq!(r.json()["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.aborted.v1~");
    // Other chats are independent.
    assert_eq!(h.send_message(U1, other, json!({"content": "elsewhere"})).await.status, 200);
    // Mutations are also blocked while a turn runs.
    let rid = h.turns(chat).await[0].request_id;
    let r = h.call(U1, "POST", &format!("/chats/{chat}/turns/{rid}/retry"), None).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(r.json()["context"]["violations"][0]["type"], "STATE");
    tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into()).unwrap();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "done").await);
    // Terminal -> accepted.
    let r = h.send_message(U1, chat, json!({"content": "next"})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.event_names().last().unwrap(), "done");

    // Concurrent sends: exactly one wins.
    let chat3 = h.create_chat(U1, None).await;
    let (tx1, _d1) = h.provider.push_channel();
    let (tx2, _d2) = h.provider.push_channel();
    let path = format!("/chats/{chat3}/messages:stream");
    let (a, b) = tokio::join!(
        h.open(U1, "POST", &path, Some(json!({"content": "a"}))),
        h.open(U1, "POST", &path, Some(json!({"content": "b"})))
    );
    let statuses = [a.0, b.0];
    assert!(statuses.contains(&200) && statuses.contains(&409), "{statuses:?}");
    for tx in [tx1, tx2] {
        let _ = tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into());
    }
    drop((a, b));
    assert_eq!(h.turns(chat3).await.len(), 1);
}
