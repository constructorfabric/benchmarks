//! Idempotent replay, request-id conflicts, replay-before-guard ordering and
//! the one-running-turn-per-chat guard.

mod common;

use common::*;
use futures::StreamExt;
use http::StatusCode;
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn replay_returns_stored_result_without_side_effects() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let rid = Uuid::new_v4();
    h.gw.push(text_reply(&["The ", "answer"], 40, 8));
    let first = h
        .send_body(&a, chat, json!({"content": "question", "request_id": rid}))
        .await;
    assert_eq!(first.names().last(), Some(&"done"));
    h.drain_outbox().await;
    let calls_before = h.gw.chat_calls().len();
    let quota_before = h.quota(USER_A, "total", "daily").await;
    let msgs_before = h.messages(&a, chat).await.len();
    let published_before = h.policy.published.lock().len();

    let replay = h
        .send_body(&a, chat, json!({"content": "question", "request_id": rid}))
        .await;
    assert_eq!(replay.status, StatusCode::OK);
    let names = replay.names();
    assert_eq!(names.first(), Some(&"stream_started"));
    assert_eq!(names.last(), Some(&"done"));
    let started = replay.first("stream_started").unwrap();
    assert_eq!(started["is_new_turn"], false);
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["message_id"], first.message_id().to_string());
    assert_eq!(replay.text(), "The answer");
    let done = replay.first("done").unwrap();
    assert_eq!(done["usage"]["input_tokens"], 40);
    assert_eq!(done["usage"]["output_tokens"], 8);
    assert_eq!(done["effective_model"], "gpt-premium");

    h.drain_outbox().await;
    assert_eq!(h.gw.chat_calls().len(), calls_before, "no provider call on replay");
    assert_eq!(h.quota(USER_A, "total", "daily").await, quota_before, "no quota change");
    assert_eq!(h.messages(&a, chat).await.len(), msgs_before, "no new messages");
    assert_eq!(h.policy.published.lock().len(), published_before, "no usage event");
}

#[tokio::test(flavor = "multi_thread")]
async fn request_id_conflicts() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;

    // failed turn → reuse is a conflict
    let rid_failed = Uuid::new_v4();
    h.gw.push(Reply::Status(500, json!({"error": {"message": "x"}})));
    let s = h
        .send_body(&a, chat, json!({"content": "q", "request_id": rid_failed}))
        .await;
    assert_eq!(s.first("error").unwrap()["code"], "provider_error");
    let again = h
        .send_body(&a, chat, json!({"content": "q", "request_id": rid_failed}))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.problem["context"]["reason"], "request_id_conflict");
    assert_eq!(again.problem["title"], "Aborted");

    // deleted turn → conflict as well
    let rid_del = Uuid::new_v4();
    let s = h.send_body(&a, chat, json!({"content": "q2", "request_id": rid_del})).await;
    assert_eq!(s.names().last(), Some(&"done"));
    let d = h
        .req("DELETE", &format!("/mini-chat/v1/chats/{chat}/turns/{rid_del}"), &a, None)
        .await;
    assert_eq!(d.status, StatusCode::NO_CONTENT, "{}", d.text);
    let again = h.send_body(&a, chat, json!({"content": "q2", "request_id": rid_del})).await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.problem["context"]["reason"], "request_id_conflict");

    // the same request id in another chat is independent
    let other = h.create_chat(&a).await;
    let s = h.send_body(&a, other, json!({"content": "q", "request_id": rid_failed})).await;
    assert_eq!(s.names().last(), Some(&"done"), "{}", s.raw);
}

#[tokio::test(flavor = "multi_thread")]
async fn running_turn_blocks_parallel_turns_and_running_request_id_conflicts() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let tx = h.gw.push_channel();
    tx.send(created_frame()).unwrap();
    tx.send(delta_frame("working")).unwrap();
    let rid = Uuid::new_v4();
    let resp = h
        .open_stream(
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            &a,
            json!({"content": "long", "request_id": rid}),
        )
        .await;
    let mut body = resp.into_body().into_data_stream();
    let mut seen = String::new();
    while let Some(Ok(c)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&c));
        if seen.contains("working") {
            break;
        }
    }
    // another turn while running → 409 turn_already_running
    let s = h.send(&a, chat, "parallel").await;
    assert_eq!(s.status, StatusCode::CONFLICT, "{}", s.raw);
    assert_eq!(s.problem["context"]["reason"], "turn_already_running");
    // the running turn's request id → 409 request_id_conflict
    let s = h.send_body(&a, chat, json!({"content": "long", "request_id": rid})).await;
    assert_eq!(s.status, StatusCode::CONFLICT);
    assert_eq!(s.problem["context"]["reason"], "request_id_conflict");
    // other chats are unaffected
    let other = h.create_chat(&a).await;
    let s = h.send(&a, other, "elsewhere").await;
    assert_eq!(s.names().last(), Some(&"done"));
    // status while running
    let st = h.get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), &a).await;
    assert_eq!(st.body["state"], "running");
    assert!(st.body.get("assistant_message_id").is_none());

    // finish the running turn; a new turn is then accepted
    tx.send(completed_frame(1, 1)).unwrap();
    drop(tx);
    while let Some(Ok(c)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&c));
    }
    assert!(seen.contains("event: done"));
    let s = h.send(&a, chat, "next").await;
    assert_eq!(s.names().last(), Some(&"done"), "{}", s.raw);
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_is_checked_before_parallel_turn_guard() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let done_rid = Uuid::new_v4();
    let s = h.send_body(&a, chat, json!({"content": "one", "request_id": done_rid})).await;
    assert_eq!(s.names().last(), Some(&"done"));
    // start a second, long-running turn
    let tx = h.gw.push_channel();
    tx.send(created_frame()).unwrap();
    let resp = h
        .open_stream(&format!("/mini-chat/v1/chats/{chat}/messages:stream"), &a, json!({"content": "two"}))
        .await;
    let mut body = resp.into_body().into_data_stream();
    let mut seen = String::new();
    while let Some(Ok(c)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&c));
        if seen.contains("stream_started") {
            break;
        }
    }
    // replay of the completed turn succeeds even though a turn is running
    let replay = h.send_body(&a, chat, json!({"content": "one", "request_id": done_rid})).await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.raw);
    assert_eq!(replay.first("stream_started").unwrap()["is_new_turn"], false);
    drop(tx);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_sends_admit_exactly_one() {
    let h = std::sync::Arc::new(Harness::new().await);
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let mut senders = Vec::new();
    for _ in 0..4 {
        senders.push(h.gw.push_channel());
    }
    let mut tasks = Vec::new();
    for i in 0..4 {
        let h = std::sync::Arc::clone(&h);
        let a = a.clone();
        tasks.push(tokio::spawn(async move {
            h.open_stream(
                &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
                &a,
                json!({"content": format!("c{i}")}),
            )
            .await
        }));
    }
    let mut ok = 0;
    let mut conflict = 0;
    let mut open = Vec::new();
    for t in tasks {
        let resp = t.await.unwrap();
        match resp.status() {
            StatusCode::OK => ok += 1,
            StatusCode::CONFLICT => conflict += 1,
            other => panic!("unexpected {other}"),
        }
        open.push(resp);
    }
    let dbg = h.rows(&format!("SELECT state, error_code, hex(request_id) FROM chat_turns WHERE chat_id = {}", blob(chat))).await;
    assert_eq!(ok, 1, "exactly one turn runs: {dbg:?}");
    assert_eq!(conflict, 3);
    assert_eq!(
        h.scalar(&format!(
            "SELECT COUNT(*) FROM chat_turns WHERE chat_id = {} AND state = 'running'",
            blob(chat)
        ))
        .await,
        1
    );
    drop(senders);
    drop(open);
}
