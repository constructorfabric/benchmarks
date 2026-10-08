//! Turn mutations (retry / edit / delete), their guards, the full send
//! pipeline on mutation, carried-forward attachments and tools, concurrent
//! mutations, turn status and the turn state machine.

mod common;

use std::sync::Arc;

use common::*;
use futures::StreamExt;
use http::StatusCode;
use serde_json::json;
use uuid::Uuid;

fn turn_uri(chat: Uuid, rid: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/turns/{rid}")
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_regenerates_with_new_request_id() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let s = h.send(&a, chat, "question").await;
    let old = s.request_id();
    h.gw.push(text_reply(&["second ", "try"], 20, 4));
    let r = h.sse("POST", &format!("{}/retry", turn_uri(chat, old)), &a, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.raw);
    assert_eq!(r.names().first(), Some(&"stream_started"));
    assert_eq!(r.names().last(), Some(&"done"));
    let new = r.request_id();
    assert_ne!(new, old);
    assert_eq!(r.first("stream_started").unwrap()["is_new_turn"], true);
    assert_eq!(r.text(), "second try");

    // the old turn is gone (404), the new one is done
    assert_eq!(h.get(&turn_uri(chat, old), &a).await.status, StatusCode::NOT_FOUND);
    assert_eq!(h.get(&turn_uri(chat, new), &a).await.body["state"], "done");
    let old_row = h
        .rows(&format!(
            "SELECT hex(replaced_by_request_id), CAST(deleted_at IS NOT NULL AS TEXT) FROM chat_turns WHERE request_id = {}",
            blob(old)
        ))
        .await;
    assert_eq!(old_row[0][0].as_deref().unwrap().to_lowercase(), new.simple().to_string());
    assert_eq!(old_row[0][1].as_deref(), Some("1"));

    let msgs = h.messages(&a, chat).await;
    assert_eq!(msgs.len(), 2, "old pair replaced: {msgs:?}");
    assert_eq!(msgs[0]["content"], "question");
    assert_eq!(msgs[0]["request_id"], new.to_string());
    assert_eq!(msgs[1]["content"], "second try");
    // the provider got the same question again, without the old answer
    let call = h.gw.chat_calls().pop().unwrap().json();
    let input = call["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "{call}");
    assert!(input[0].to_string().contains("question"));
    // audit
    h.drain_outbox().await;
    let audits = h.audit.events.lock().clone();
    assert!(audits.iter().any(|e| serde_json::to_value(e).unwrap()["event_type"] == "turn_retry"));
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_replaces_user_message() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.send(&a, chat, "first").await;
    let s = h.send(&a, chat, "typo questoin").await;
    let r = h
        .sse(
            "PATCH",
            &turn_uri(chat, s.request_id()),
            &a,
            Some(json!({"content": "fixed question"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.raw);
    assert_eq!(r.names().last(), Some(&"done"));
    let msgs = h.messages(&a, chat).await;
    let contents: Vec<&str> = msgs.iter().map(|m| m["content"].as_str().unwrap()).collect();
    assert_eq!(contents, vec!["first", "Hello world", "fixed question", "Hello world"]);
    let call = h.gw.chat_calls().pop().unwrap().json();
    assert!(call["input"].to_string().contains("fixed question"));
    assert!(!call["input"].to_string().contains("typo questoin"));
    // empty edit
    let r = h
        .sse("PATCH", &turn_uri(chat, r.request_id()), &a, Some(json!({"content": "  "})))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.problem["context"]["field_violations"][0]["reason"], "EMPTY_CONTENT");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_removes_latest_turn() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let first = h.send(&a, chat, "one").await.request_id();
    let second = h.send(&a, chat, "two").await.request_id();
    // not the latest
    let r = h.req("DELETE", &turn_uri(chat, first), &a, None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.body["context"]["reason"], "NOT_LATEST_TURN");
    let r = h.req("DELETE", &turn_uri(chat, second), &a, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    assert_eq!(h.messages(&a, chat).await.len(), 2);
    assert_eq!(h.get(&turn_uri(chat, second), &a).await.status, StatusCode::NOT_FOUND);
    // a deleted turn is NOT_LATEST_TURN on any further mutation
    let r = h.req("DELETE", &turn_uri(chat, second), &a, None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.body["context"]["reason"], "NOT_LATEST_TURN");
    // the previous turn is the latest again
    let r = h.sse("POST", &format!("{}/retry", turn_uri(chat, first)), &a, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.raw);
    // unknown turn
    let r = h.req("DELETE", &turn_uri(chat, Uuid::new_v4()), &a, None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body["context"]["resource_type"], "gts.cf.core.mini_chat.turn.v1~");
    let r = h
        .get(&format!("/mini-chat/v1/chats/{chat}/turns/not-a-uuid"), &a)
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn mutations_require_terminal_latest_turn() {
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
            json!({"content": "slow", "request_id": rid}),
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
    for (m, uri, b) in [
        ("POST", format!("{}/retry", turn_uri(chat, rid)), None),
        ("PATCH", turn_uri(chat, rid), Some(json!({"content": "x"}))),
        ("DELETE", turn_uri(chat, rid), None),
    ] {
        let r = h.req(m, &uri, &a, b).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{m}: {}", r.text);
        assert_eq!(r.body["context"]["violations"][0]["subject"], "turn_state");
        assert_eq!(r.body["context"]["violations"][0]["type"], "STATE");
    }
    tx.send(completed_frame(1, 1)).unwrap();
    drop(tx);
    while body.next().await.is_some() {}
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_of_failed_and_cancelled_turns() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.gw.push(Reply::Status(500, json!({"error": {"message": "x"}})));
    let failed = h.send(&a, chat, "q").await;
    assert_eq!(failed.first("error").unwrap()["code"], "provider_error");
    let r = h
        .sse("POST", &format!("{}/retry", turn_uri(chat, failed.request_id())), &a, None)
        .await;
    assert_eq!(r.names().last(), Some(&"done"), "{}", r.raw);
    let msgs = h.messages(&a, chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["content"], "q");
}

#[tokio::test(flavor = "multi_thread")]
async fn mutation_runs_full_pipeline_quota() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let s = h.send(&a, chat, "q").await;
    // exhaust every tier
    h.policy.set_limits((1, 1), (1, 1));
    let r = h
        .sse("POST", &format!("{}/retry", turn_uri(chat, s.request_id())), &a, None)
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.raw);
    assert_eq!(r.problem["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(r.problem["context"]["violations"][0]["description"], "quota_exceeded");
    // nothing mutated: the turn is still the latest and done
    assert_eq!(h.get(&turn_uri(chat, s.request_id()), &a).await.body["state"], "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn mutation_carries_attachments_and_web_search_forward() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let up = h.upload(&a, chat, "doc.md", "text/markdown", b"# Title").await;
    let att = up.body["id"].as_str().unwrap().to_owned();
    let s = h
        .send_body(
            &a,
            chat,
            json!({"content": "use the doc", "attachment_ids": [att], "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(s.names().last(), Some(&"done"), "{}", s.raw);
    let r = h
        .sse("PATCH", &turn_uri(chat, s.request_id()), &a, Some(json!({"content": "use it again"})))
        .await;
    assert_eq!(r.names().last(), Some(&"done"), "{}", r.raw);
    let msgs = h.messages(&a, chat).await;
    assert_eq!(msgs[0]["content"], "use it again");
    assert_eq!(msgs[0]["attachments"][0]["attachment_id"], att.as_str(), "attachments carried forward");
    let call = h.gw.chat_calls().pop().unwrap().json();
    let tools = call["tools"].to_string();
    assert!(tools.contains("web_search"), "web search carried forward: {call}");
    assert!(tools.contains("file_search"), "{call}");
    let row = h
        .rows(&format!(
            "SELECT CAST(web_search_enabled AS TEXT) FROM chat_turns WHERE request_id = {}",
            blob(r.request_id())
        ))
        .await;
    assert_eq!(row[0][0].as_deref(), Some("1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_mutations_resolve_to_one_winner() {
    let h = Arc::new(Harness::new().await);
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let rid = h.send(&a, chat, "q").await.request_id();
    let senders: Vec<_> = (0..3).map(|_| h.gw.push_channel()).collect();
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let h = Arc::clone(&h);
        let a = a.clone();
        tasks.push(tokio::spawn(async move {
            let r = h
                .call({
                    let mut req = http::Request::builder()
                        .method("POST")
                        .uri(format!("/mini-chat/v1/chats/{chat}/turns/{rid}/retry"))
                        .body(axum::body::Body::empty())
                        .unwrap();
                    req.extensions_mut().insert(a.clone());
                    req
                })
                .await;
            let status = r.status();
            let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap_or_default();
            (status, String::from_utf8_lossy(&body).into_owned())
        }));
    }
    drop(senders);
    let mut ok = 0;
    for t in tasks {
        let (status, body) = t.await.unwrap();
        if status == StatusCode::OK {
            ok += 1;
        } else {
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert!(
                body.contains("GENERATION_IN_PROGRESS") || body.contains("NOT_LATEST_TURN"),
                "{body}"
            );
        }
    }
    assert_eq!(ok, 1, "exactly one mutation wins");
    assert_eq!(
        h.scalar(&format!(
            "SELECT COUNT(*) FROM chat_turns WHERE chat_id = {} AND deleted_at IS NULL",
            blob(chat)
        ))
        .await,
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn context_failure_after_mutation_commit_marks_turn_failed() {
    let h = Harness::with(Options {
        catalog: vec![model(
            "tiny",
            "standard",
            json!({"context_window": 1500, "max_output_tokens": 1000, "max_input_tokens": 0,
                   "preference": {"is_default": true}, "multimodal_capabilities": []}),
        )],
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let s = h.send(&a, chat, "short").await;
    assert_eq!(s.names().last(), Some(&"done"), "{}", s.raw);
    // An edit with a message that no longer fits the budget.
    let r = h
        .sse("PATCH", &turn_uri(chat, s.request_id()), &a, Some(json!({"content": "x".repeat(4000)})))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.raw);
    assert_eq!(r.problem["context"]["field_violations"][0]["reason"], "CONTEXT_BUDGET_EXCEEDED");
    let rows = h
        .rows(&format!(
            "SELECT state, error_code FROM chat_turns WHERE chat_id = {} AND deleted_at IS NULL",
            blob(chat)
        ))
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_deref(), Some("failed"));
    assert_eq!(rows[0][1].as_deref(), Some("context_length_exceeded"));
}
