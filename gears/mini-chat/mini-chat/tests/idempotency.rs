//! T031: idempotent replay, `request_id` conflicts, parallel-turn guard.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use tokio::sync::Notify;
use uuid::Uuid;

#[tokio::test]
async fn replay_returns_stored_result_without_side_effects() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["Stored ".into(), "answer".into()],
        usage: (100, 20),
        before_done: vec![],
        output: None,
    });
    let rid = Uuid::new_v4();
    let first = h
        .send_body(chat, json!({"content": "q", "request_id": rid}))
        .await;
    assert_eq!(first.names().last(), Some(&"done"));
    let msg_id = h.messages(chat).await[1]["id"].clone();
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let provider_calls = h.provider.chat_requests().len();
    let quota_before = db::quota_rows(&h).await;
    let published_before = h.policy.published().len();
    let audit_before = h.audit_events().len();

    let replay = h
        .send_body(
            chat,
            json!({"content": "different content ignored", "request_id": rid}),
        )
        .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.names(), vec!["stream_started", "delta", "done"]);
    let st = replay.first("stream_started").unwrap();
    assert_eq!(st["is_new_turn"], false);
    assert_eq!(st["request_id"], rid.to_string());
    assert_eq!(st["message_id"], msg_id);
    assert_eq!(replay.text(), "Stored answer");
    let done = replay.first("done").unwrap();
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 100, "output_tokens": 20})
    );
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_reason").is_none());
    assert!(done.get("quota_warnings").is_none());

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        h.provider.chat_requests().len(),
        provider_calls,
        "no provider call on replay"
    );
    let quota_after = db::quota_rows(&h).await;
    let key = |r: &mini_chat::infra::db::entities::quota_usage::Model| {
        (
            r.bucket.clone(),
            r.period_type.clone(),
            r.spent_credits_micro,
            r.reserved_credits_micro,
            r.calls,
        )
    };
    assert_eq!(
        quota_before.iter().map(key).collect::<Vec<_>>(),
        quota_after.iter().map(key).collect::<Vec<_>>()
    );
    assert_eq!(h.policy.published().len(), published_before);
    assert_eq!(h.audit_events().len(), audit_before);
    assert_eq!(h.messages(chat).await.len(), 2, "no new messages");
    assert_eq!(db::turns(&h, chat).await.len(), 1);
}

#[tokio::test]
async fn request_id_conflict_for_failed_cancelled_and_deleted_turns() {
    let h = Harness::new().await;
    // failed
    let chat = h.create_chat().await;
    h.provider.push(Script::Failed {
        parts: vec![],
        message: "x".into(),
        usage: None,
    });
    let rid = Uuid::new_v4();
    h.send_body(chat, json!({"content": "q", "request_id": rid}))
        .await;
    let r = h
        .send_body(chat, json!({"content": "q", "request_id": rid}))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{r:?}");
    assert_reason(&r.error, "request_id_conflict");

    // cancelled
    let chat2 = h.create_chat().await;
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["p".into()],
        gate,
        usage: (1, 1),
    });
    let rid2 = Uuid::new_v4();
    let mut s = h
        .open_send(chat2, json!({"content": "q", "request_id": rid2}))
        .await;
    assert!(s.until("delta").await);
    drop(s);
    h.wait_turn_terminal(chat2).await;
    let r = h
        .send_body(chat2, json!({"content": "q", "request_id": rid2}))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_reason(&r.error, "request_id_conflict");

    // completed then deleted
    let chat3 = h.create_chat().await;
    let rid3 = Uuid::new_v4();
    h.send_body(chat3, json!({"content": "q", "request_id": rid3}))
        .await;
    let (s, _, _) = h
        .req(
            &h.ctx(),
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat3}/turns/{rid3}"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let r = h
        .send_body(chat3, json!({"content": "q", "request_id": rid3}))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_reason(&r.error, "request_id_conflict");
}

#[tokio::test]
async fn one_running_turn_per_chat_and_replay_checked_first() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    // a completed turn to replay later
    let done_rid = Uuid::new_v4();
    h.send_body(chat, json!({"content": "first", "request_id": done_rid}))
        .await;

    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["x".into()],
        gate: gate.clone(),
        usage: (1, 1),
    });
    let running_rid = Uuid::new_v4();
    let mut s = h
        .open_send(
            chat,
            json!({"content": "second", "request_id": running_rid}),
        )
        .await;
    assert!(s.until("delta").await);

    // a different request while one is running -> 409 turn_already_running, no provider call
    let calls = h.provider.chat_requests().len();
    let r = h.send(chat, "third").await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_reason(&r.error, "turn_already_running");
    // the running turn's own request id -> request_id_conflict (not running guard)
    let r = h
        .send_body(
            chat,
            json!({"content": "second", "request_id": running_rid}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_reason(&r.error, "request_id_conflict");
    // replay of the completed turn still works while another turn runs
    let r = h
        .send_body(chat, json!({"content": "first", "request_id": done_rid}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.first("stream_started").unwrap()["is_new_turn"], false);
    assert_eq!(h.provider.chat_requests().len(), calls);
    // other chats are not affected
    let other = h.create_chat().await;
    let r = h.send(other, "independent").await;
    assert_eq!(r.names().last(), Some(&"done"));

    gate.notify_one();
    assert!(s.until("done").await);
    h.wait_turn_terminal(chat).await;
    // accepted again once the previous turn is terminal
    let r = h.send(chat, "fourth").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.names().last(), Some(&"done"));
}

#[tokio::test]
async fn concurrent_sends_only_one_wins() {
    let h = Arc::new(Harness::new().await);
    let chat = h.create_chat().await;
    let gate = Arc::new(Notify::new());
    for _ in 0..4 {
        h.provider.push(Script::Gated {
            parts: vec!["x".into()],
            gate: gate.clone(),
            usage: (1, 1),
        });
    }
    let mut handles = Vec::new();
    for i in 0..4 {
        let h = h.clone();
        handles.push(tokio::spawn(async move {
            let mut s = h.open_send(chat, json!({"content": format!("c{i}")})).await;
            let status = s.status;
            if status == StatusCode::OK {
                s.until("delta").await;
            }
            (status, s)
        }));
    }
    let mut ok = 0;
    let mut streams = Vec::new();
    for hd in handles {
        let (st, s) = hd.await.unwrap();
        if st == StatusCode::OK {
            ok += 1;
        } else {
            assert_eq!(st, StatusCode::CONFLICT);
        }
        streams.push(s);
    }
    assert_eq!(ok, 1);
    gate.notify_waiters();
    gate.notify_one();
    drop(streams);
    let turns = h.wait_turn_terminal(chat).await;
    assert_eq!(turns.len(), 1);
}
