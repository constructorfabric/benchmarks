//! T029/T032/T035: send-message streaming, SSE contract, persistence, provider errors, cancellation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::let_underscore_must_use
)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use serde_json::{Value, json};
use tokio::sync::Notify;
use uuid::Uuid;

#[tokio::test]
async fn stream_event_order_and_persistence() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["Hel".into(), "lo".into(), " there".into()],
        usage: (120, 30),
        before_done: vec![],
        output: None,
    });
    let rid = Uuid::new_v4();
    let r = h
        .send_body(chat, json!({"content": "Hi!", "request_id": rid}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.names(),
        vec!["stream_started", "delta", "delta", "delta", "done"]
    );
    let started = r.first("stream_started").unwrap();
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["is_new_turn"], true);
    assert!(started.get("thread_summary_applied").is_none());
    let message_id = started["message_id"].as_str().unwrap().to_owned();
    assert_eq!(r.text(), "Hello there");
    let done = r.first("done").unwrap();
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 120, "output_tokens": 30})
    );
    assert_eq!(done["effective_model"], "prem");
    assert_eq!(done["selected_model"], "prem");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_from").is_none());
    assert!(
        done.get("message_id").is_none() && done.get("turn_id").is_none(),
        "no internal ids: {done}"
    );
    let w = done["quota_warnings"].as_array().expect("quota_warnings");
    assert!(
        w.iter()
            .any(|x| x["tier"] == "total" && x["period"] == "daily")
    );
    assert!(w.iter().any(|x| x["tier"] == "premium"));

    // persisted assistant message (id announced up front) with usage
    let msgs = h.messages(chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "Hi!");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["id"], message_id.as_str());
    assert_eq!(msgs[1]["content"], "Hello there");
    assert_eq!(msgs[1]["input_tokens"], 120);
    assert_eq!(msgs[1]["output_tokens"], 30);
    assert_eq!(msgs[1]["model"], "prem");
    assert_eq!(msgs[0]["request_id"], rid.to_string());
    assert_eq!(msgs[1]["request_id"], rid.to_string());

    let turns = db::turns(&h, chat).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].state, "completed");
    assert_eq!(
        turns[0].assistant_message_id.unwrap().to_string(),
        message_id
    );
    assert_eq!(
        turns[0].provider_response_id.as_deref(),
        Some("resp_abcdef")
    );

    // usage published exactly once with actual settlement
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let ev = h.published_for(rid);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].terminal_state, "completed");
    assert_eq!(ev[0].billing_outcome, "completed");
    assert_eq!(ev[0].settlement_method, "actual");
    assert_eq!(ev[0].actual_credits_micro, 120 + 2 * 30);
    assert_eq!(
        ev[0].dedupe_key,
        format!(
            "{}/{}/{}",
            h.tenant.simple(),
            turns[0].id.simple(),
            rid.simple()
        )
    );
    // audit turn_completed
    assert!(h.eventually(|| !h.audit_events().is_empty()).await);
}

#[tokio::test]
async fn generated_request_id_and_sse_headers() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let r = h.send(chat, "again").await;
    let rid = r.request_id();
    assert_ne!(rid, Uuid::nil());
    let (st, v) = h
        .get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["state"], "done");
    let mut req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/messages:stream"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(json!({"content": "h"}).to_string()))
        .unwrap();
    req.extensions_mut().insert(h.ctx());
    let resp = tower::ServiceExt::oneshot(h.router.clone(), req)
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert_eq!(resp.headers()["cache-control"], "no-cache");
    let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
}

#[tokio::test]
async fn deltas_are_relayed_before_provider_finishes() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["first ".into()],
        gate: gate.clone(),
        usage: (10, 5),
    });
    let mut s = h.open_send(chat, json!({"content": "go"})).await;
    assert!(
        s.until("delta").await,
        "delta must arrive while provider is still streaming: {:?}",
        s.names()
    );
    assert!(!s.names().contains(&"done"));
    // the turn is still running while the client already has the delta
    let turns = db::turns(&h, chat).await;
    assert_eq!(turns[0].state, "running");
    gate.notify_one();
    assert!(s.until("done").await);
    assert_eq!(s.names(), vec!["stream_started", "delta", "done"]);
}

#[tokio::test]
async fn ping_is_sent_before_first_delta() {
    let mut cfg = default_config();
    cfg["streaming"] = json!({"sse_ping_interval_seconds": 5});
    let h = Harness::with(Opts {
        config: cfg,
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec![],
        gate: gate.clone(),
        usage: (1, 1),
    });
    let mut s = h.open_send(chat, json!({"content": "slow"})).await;
    assert!(s.until("ping").await, "{:?}", s.names());
    assert_eq!(
        s.events.iter().find(|(n, _)| n == "ping").unwrap().1,
        json!({})
    );
    gate.notify_one();
    assert!(s.until("done").await);
    let names = s.names();
    assert_eq!(names[0], "stream_started");
    assert_eq!(*names.last().unwrap(), "done");
}

#[tokio::test]
async fn tool_events_and_citations_ordering() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let output = json!([{"type": "message", "content": [{"type": "output_text", "text": "See example.",
        "annotations": [{"type": "url_citation", "url": "https://example.com/a", "title": "Example", "start_index": 4, "end_index": 11}]}]}]);
    h.provider.push(Script::Ok {
        parts: vec!["See ".into(), "example.".into()],
        usage: (50, 10),
        before_done: vec![
            (
                "response.web_search_call.searching".into(),
                json!({"type": "response.web_search_call.searching"}),
            ),
            (
                "response.web_search_call.completed".into(),
                json!({"type": "response.web_search_call.completed"}),
            ),
        ],
        output: Some(output),
    });
    let r = h
        .send_body(
            chat,
            json!({"content": "search", "web_search": {"enabled": true}}),
        )
        .await;
    let names = r.names();
    assert_eq!(names.first(), Some(&"stream_started"));
    assert_eq!(names.last(), Some(&"done"));
    let ci = names
        .iter()
        .position(|n| *n == "citations")
        .expect("citations");
    assert_eq!(
        ci,
        names.len() - 2,
        "citations right before done: {names:?}"
    );
    let tools: Vec<&Value> = r
        .events
        .iter()
        .filter(|(n, _)| n == "tool")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["phase"], "start");
    assert_eq!(tools[0]["name"], "web_search");
    assert_eq!(tools[1]["phase"], "done");
    let c = &r.first("citations").unwrap()["items"][0];
    assert_eq!(c["source"], "web");
    assert_eq!(c["url"], "https://example.com/a");
    assert_eq!(c["title"], "Example");
    assert_eq!(c["span"], json!({"start": 4, "end": 11}));
    assert!(c.get("score").is_none());
}

#[tokio::test]
async fn provider_failed_event_is_sanitized_terminal_error() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Failed {
        parts: vec!["partial".into()],
        message: "boom for resp_abc123def456 at https://internal.example/x key sk-abcdefghijklmnop"
            .into(),
        usage: Some((40, 3)),
    });
    let rid = Uuid::new_v4();
    let r = h
        .send_body(chat, json!({"content": "x", "request_id": rid}))
        .await;
    assert_eq!(r.names().last(), Some(&"error"));
    assert!(!r.names().contains(&"done"));
    let e = r.first("error").unwrap();
    assert_eq!(e["code"], "provider_error");
    let msg = e["message"].as_str().unwrap();
    assert!(
        !msg.contains("resp_abc123def456") && !msg.contains("https://") && !msg.contains("sk-abc"),
        "{msg}"
    );
    assert!(
        msg.contains("[provider_id]") && msg.contains("[url]") && msg.contains("[credential]"),
        "{msg}"
    );

    let turns = h.wait_turn_terminal(chat).await;
    assert_eq!(turns[0].state, "failed");
    assert_eq!(turns[0].error_code.as_deref(), Some("provider_error"));
    // failed turn: no assistant message persisted
    let msgs = h.messages(chat).await;
    assert_eq!(msgs.len(), 1);
    let (_, v) = h
        .get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"))
        .await;
    assert_eq!(v["state"], "error");
    assert_eq!(v["error_code"], "provider_error");
    assert!(v.get("assistant_message_id").is_none());
    // usage present -> actual settlement
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    let ev = &h.published_for(rid)[0];
    assert_eq!(ev.billing_outcome, "failed");
    assert_eq!(ev.settlement_method, "actual");
    assert_eq!(ev.actual_credits_micro, 40 + 6);
}

#[tokio::test]
async fn provider_failure_without_usage_is_estimated() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Failed {
        parts: vec![],
        message: "bad".into(),
        usage: None,
    });
    let rid = Uuid::new_v4();
    let r = h
        .send_body(chat, json!({"content": "x", "request_id": rid}))
        .await;
    assert_eq!(r.first("error").unwrap()["code"], "provider_error");
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    let ev = &h.published_for(rid)[0];
    assert_eq!(ev.settlement_method, "estimated");
    assert!(ev.usage.is_none());
    assert!(ev.actual_credits_micro > 0);
}

#[tokio::test]
async fn provider_http_429_maps_to_rate_limited() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Http {
        status: 429,
        body: json!({"error": {"message": "slow down"}}),
        retry_after: Some(7),
    });
    let r = h.send(chat, "x").await;
    assert_eq!(r.names(), vec!["stream_started", "error"]);
    let e = r.first("error").unwrap();
    assert_eq!(e["code"], "rate_limited");
    assert!(e["message"].as_str().unwrap().contains('7'), "{e}");
    let t = h.wait_turn_terminal(chat).await;
    assert_eq!(t[0].state, "failed");
}

#[tokio::test]
async fn provider_http_500_maps_to_provider_error() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Http {
        status: 500,
        body: json!({"error": {"message": "internal"}}),
        retry_after: None,
    });
    let r = h.send(chat, "x").await;
    assert_eq!(r.first("error").unwrap()["code"], "provider_error");
}

#[tokio::test]
async fn stream_without_terminal_event_is_provider_error() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Truncated {
        parts: vec!["abc".into()],
    });
    let r = h.send(chat, "x").await;
    assert_eq!(r.names().last(), Some(&"error"));
    assert_eq!(r.first("error").unwrap()["code"], "provider_error");
    let t = h.wait_turn_terminal(chat).await;
    assert_eq!(t[0].state, "failed");
}

#[tokio::test]
async fn client_disconnect_cancels_turn_and_keeps_partial_text() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["partial ".into(), "answer".into()],
        gate,
        usage: (10, 10),
    });
    let rid = Uuid::new_v4();
    let mut s = h
        .open_send(chat, json!({"content": "go", "request_id": rid}))
        .await;
    assert!(s.until("delta").await);
    // wait for the second delta to be relayed too
    for _ in 0..50 {
        if s.events.iter().filter(|(n, _)| n == "delta").count() >= 2 {
            break;
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), s.until("never")).await;
    }
    drop(s);
    let turns = h.wait_turn_terminal(chat).await;
    assert_eq!(turns[0].state, "cancelled");
    assert!(turns[0].error_code.is_none());
    let msgs = h.messages(chat).await;
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert_eq!(msgs[1]["content"], "partial answer");
    let (_, v) = h
        .get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"))
        .await;
    assert_eq!(v["state"], "cancelled");
    assert_eq!(v["assistant_message_id"], msgs[1]["id"]);
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    let ev = &h.published_for(rid)[0];
    assert_eq!(ev.terminal_state, "cancelled");
    assert_eq!(ev.billing_outcome, "aborted");
    assert_eq!(ev.settlement_method, "estimated");
    assert!(ev.usage.is_none());
}

#[tokio::test]
async fn cancel_before_any_text_persists_no_message() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec![],
        gate,
        usage: (10, 10),
    });
    let mut s = h.open_send(chat, json!({"content": "go"})).await;
    assert!(s.until("stream_started").await);
    // make sure the provider call has been made before disconnecting
    assert!(
        h.eventually(|| !h.provider.chat_requests().is_empty())
            .await
    );
    drop(s);
    let turns = h.wait_turn_terminal(chat).await;
    assert_eq!(turns[0].state, "cancelled");
    assert!(turns[0].assistant_message_id.is_none());
    assert_eq!(h.messages(chat).await.len(), 1);
}

#[tokio::test]
async fn empty_completion_still_persists_assistant_message() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec![],
        usage: (0, 0),
        before_done: vec![],
        output: None,
    });
    let r = h.send(chat, "x").await;
    assert_eq!(r.names(), vec!["stream_started", "done"]);
    let msgs = h.messages(chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1]["content"], "");
    assert!(
        msgs[1].get("input_tokens").is_none(),
        "zero tokens omitted: {}",
        msgs[1]
    );
}

#[tokio::test]
async fn provider_request_shape() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let r = h.send(chat, "What is up?").await;
    assert_eq!(r.names().last(), Some(&"done"));
    let reqs = h.provider.requests();
    let chat_req = reqs
        .iter()
        .find(|r| r.path.ends_with("/responses"))
        .unwrap();
    assert_eq!(chat_req.method, "POST");
    let j = chat_req.json.as_ref().unwrap();
    assert_eq!(j["model"], "prov-prem");
    assert_eq!(j["stream"], true);
    assert!(
        j["instructions"]
            .as_str()
            .unwrap()
            .starts_with("You are prem.")
    );
    assert_eq!(j["max_output_tokens"], 1000);
    assert_eq!(j["max_tool_calls"], 2);
    assert_eq!(j["user"].as_str().unwrap().len(), 64);
    assert_eq!(j["metadata"]["chat_id"], chat.to_string());
    assert_eq!(j["metadata"]["request_type"], "chat");
    assert_eq!(j["temperature"], 0.5);
    let input = j["input"].as_array().unwrap();
    let last = input.last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(
        last["content"][0],
        json!({"type": "input_text", "text": "What is up?"})
    );
}
