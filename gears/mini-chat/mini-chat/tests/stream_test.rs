//! `messages:stream`: SSE contract and ordering, request-id correlation,
//! preflight validation before any provider call, persistence, error events
//! (terminal, sanitized), keepalive pings, unbuffered relay, cancellation.

mod common;

use std::time::Duration;

use common::*;
use futures::StreamExt;
use http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn stream_happy_path_contract_and_persistence() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let rid = Uuid::new_v4();
    h.gw.push(text_reply(&["Hel", "lo", "!"], 120, 30));
    let s = h
        .send_body(&a, chat, json!({"content": "Hi there", "request_id": rid}))
        .await;
    assert_eq!(s.status, StatusCode::OK);
    assert!(
        s.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"),
        "{:?}",
        s.headers
    );
    assert_eq!(s.headers["cache-control"], "no-cache");
    assert_eq!(s.names(), vec!["stream_started", "delta", "delta", "delta", "done"]);
    let started = s.first("stream_started").unwrap();
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["is_new_turn"], true);
    assert!(started.get("thread_summary_applied").is_none());
    assert_eq!(s.text(), "Hello!");
    for (n, v) in &s.events {
        if n == "delta" {
            assert_eq!(v["type"], "text");
        }
    }
    let done = s.first("done").unwrap();
    assert_eq!(done["usage"]["input_tokens"], 120);
    assert_eq!(done["usage"]["output_tokens"], 30);
    assert_eq!(done["effective_model"], "gpt-premium");
    assert_eq!(done["selected_model"], "gpt-premium");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_from").is_none());
    // no internal identifiers in done
    let raw = done.to_string();
    for leak in ["resp_", "provider", "credits", "tenant", "policy_version"] {
        assert!(!raw.contains(leak), "done leaks {leak}: {raw}");
    }

    // Persistence: user + assistant message, completed turn with usage
    let turn = h.turn_row(rid).await;
    assert_eq!(turn[0].as_deref(), Some("completed"));
    assert_eq!(turn[4].as_deref(), Some("gpt-premium"));
    let msgs = h
        .rows(&format!(
            "SELECT role, content, CAST(input_tokens AS TEXT), CAST(output_tokens AS TEXT), provider_response_id, model \
             FROM messages WHERE chat_id = {} ORDER BY created_at",
            blob(chat)
        ))
        .await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0][0].as_deref(), Some("user"));
    assert_eq!(msgs[0][1].as_deref(), Some("Hi there"));
    assert_eq!(msgs[1][0].as_deref(), Some("assistant"));
    assert_eq!(msgs[1][1].as_deref(), Some("Hello!"));
    assert_eq!(msgs[1][2].as_deref(), Some("120"));
    assert_eq!(msgs[1][3].as_deref(), Some("30"));
    assert_eq!(msgs[1][4].as_deref(), Some("resp_0123456789abcdef"));
    assert_eq!(msgs[1][5].as_deref(), Some("gpt-premium"));

    // Turn status API
    let t = h.get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), &a).await;
    assert_eq!(t.status, StatusCode::OK);
    assert_eq!(t.body["state"], "done");
    assert_eq!(t.body["assistant_message_id"], s.message_id().to_string());
    assert!(t.body.get("error_code").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_request_shape() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.send(&a, chat, "first").await;
    h.send(&a, chat, "second").await;
    let calls = h.gw.chat_calls();
    assert_eq!(calls.len(), 2);
    let r = &calls[1];
    assert_eq!(r.method, "POST");
    assert!(r.uri.starts_with("/api.test.local/v1/responses"), "{}", r.uri);
    let b = r.json();
    assert_eq!(b["model"], "gpt-premium-provider");
    assert_eq!(b["stream"], true);
    assert_eq!(b["store"], false);
    assert_eq!(b["max_output_tokens"], 1000);
    assert!(b["instructions"].as_str().unwrap().starts_with("You are a helpful assistant."));
    let input = b["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "history (user, assistant) + current: {b}");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(input[2]["role"], "user");
    assert!(input[2].to_string().contains("second"));
    assert_eq!(b["metadata"]["chat_id"], chat.to_string());
    assert_eq!(b["metadata"]["request_type"], "chat");
    assert_eq!(b["metadata"]["tenant_id"], TENANT_A.to_string());
    assert_eq!(b["metadata"]["user_id"], USER_A.to_string());
    assert!(b["user"].as_str().is_some_and(|u| !u.is_empty()));
    assert!(b.get("tools").is_none_or(|t| t.as_array().is_some_and(Vec::is_empty)), "no tools without documents or web search: {b}");
}

#[tokio::test(flavor = "multi_thread")]
async fn server_generates_request_id() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let s = h.send(&a, chat, "hi").await;
    let rid = s.request_id();
    let t = h.get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), &a).await;
    assert_eq!(t.status, StatusCode::OK);
    assert_eq!(t.body["request_id"], rid.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn preflight_validation_happens_before_provider_call() {
    let h = Harness::with(Options {
        catalog: vec![model(
            "small",
            "standard",
            json!({"max_input_tokens": 200, "preference": {"is_default": true}, "multimodal_capabilities": []}),
        )],
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let cases: Vec<(Value, StatusCode, &str)> = vec![
        (json!({"content": ""}), StatusCode::BAD_REQUEST, "EMPTY_CONTENT"),
        (json!({"content": "   \n"}), StatusCode::BAD_REQUEST, "EMPTY_CONTENT"),
        (json!({"content": "x".repeat(1000)}), StatusCode::BAD_REQUEST, "INPUT_TOO_LONG"),
        (
            json!({"content": "hi", "attachment_ids": [Uuid::new_v4()]}),
            StatusCode::BAD_REQUEST,
            "invalid_attachment",
        ),
    ];
    for (body, status, reason) in cases {
        let s = h.send_body(&a, chat, body.clone()).await;
        assert_eq!(s.status, status, "{body}: {}", s.raw);
        assert!(s.events.is_empty(), "no SSE stream on preflight failure");
        assert_eq!(s.problem["context"]["field_violations"][0]["reason"], reason, "{}", s.raw);
    }
    // duplicate attachment ids
    let dup = Uuid::new_v4();
    let s = h
        .send_body(&a, chat, json!({"content": "hi", "attachment_ids": [dup, dup]}))
        .await;
    assert_eq!(s.status, StatusCode::BAD_REQUEST);
    assert!(h.gw.chat_calls().is_empty(), "no provider call");
    assert_eq!(
        h.scalar(&format!("SELECT COUNT(*) FROM chat_turns WHERE chat_id = {}", blob(chat))).await,
        0
    );
    assert_eq!(
        h.scalar(&format!("SELECT COUNT(*) FROM messages WHERE chat_id = {}", blob(chat))).await,
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_model_removed_from_catalog_is_invalid_model() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.policy.update(|s| s.model_catalog.retain(|m| m.id != "gpt-premium"));
    let s = h.send(&a, chat, "hi").await;
    assert_eq!(s.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.problem["context"]["field_violations"][0]["field"], "model");
    assert_eq!(s.problem["context"]["field_violations"][0]["reason"], "INVALID_MODEL");
    assert!(h.gw.chat_calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_failure_before_stream_is_500() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.policy.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let s = h.send(&a, chat, "hi").await;
    assert_eq!(s.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(s.problem["title"], "Internal");
    assert!(!s.raw.contains("policy down"), "internal details are not exposed");
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_error_event_is_terminal_and_sanitized() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.gw.push(Reply::Sse(vec![
        created_frame(),
        delta_frame("partial"),
        frame(
            "response.failed",
            json!({"response": {"id": "resp_abcdef0123456789", "error": {"code": "server_error",
                "message": "Upstream failure for resp_abcdef0123456789 at https://internal.example.com/x key sk-abcdefghijklmnop file-abcdefghijklmnopqrstu"}}}),
        ),
    ]));
    let s = h.send(&a, chat, "hi").await;
    assert_eq!(s.status, StatusCode::OK);
    assert_eq!(s.names().last(), Some(&"error"));
    assert_eq!(s.events.iter().filter(|(n, _)| n == "error").count(), 1);
    assert!(s.first("done").is_none());
    let err = s.first("error").unwrap();
    assert_eq!(err["code"], "provider_error");
    let msg = err["message"].as_str().unwrap();
    for leak in ["resp_abcdef", "https://internal", "sk-abcdefghijklmnop", "file-abcdefghijklmnopqrstu"] {
        assert!(!msg.contains(leak), "message leaks {leak}: {msg}");
    }
    let t = h.get(&format!("/mini-chat/v1/chats/{chat}/turns/{}", s.request_id()), &a).await;
    assert_eq!(t.body["state"], "error");
    assert_eq!(t.body["error_code"], "provider_error");
    assert!(t.body.get("assistant_message_id").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_http_errors_map_to_stream_codes() {
    let h = Harness::new().await;
    let a = user_a();
    let cases: Vec<(Reply, &str)> = vec![
        (Reply::Status(500, json!({"error": {"message": "boom"}})), "provider_error"),
        (Reply::Status(429, json!({"error": {"message": "slow down"}})), "rate_limited"),
        (Reply::Gateway(504), "provider_timeout"),
        (Reply::Error(gateway_timeout()), "provider_timeout"),
        (Reply::Sse(vec![created_frame(), delta_frame("cut")]), "provider_error"),
    ];
    for (reply, code) in cases {
        let chat = h.create_chat(&a).await;
        h.gw.push(reply);
        let s = h.send(&a, chat, "hi").await;
        assert_eq!(s.status, StatusCode::OK, "{code}");
        assert_eq!(s.names().last(), Some(&"error"), "{code}: {}", s.raw);
        assert_eq!(s.first("error").unwrap()["code"], code);
        let t = h.turn_row(s.request_id()).await;
        assert_eq!(t[0].as_deref(), Some("failed"));
        assert_eq!(t[1].as_deref(), Some(code));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_relays_before_provider_finishes() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let tx = h.gw.push_channel();
    tx.send(created_frame()).unwrap();
    tx.send(delta_frame("first-chunk")).unwrap();
    let resp = h
        .open_stream(&format!("/mini-chat/v1/chats/{chat}/messages:stream"), &a, json!({"content": "hi"}))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body().into_data_stream();
    let mut seen = String::new();
    // The first delta arrives while the provider stream is still open.
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(chunk)) = body.next().await {
            seen.push_str(&String::from_utf8_lossy(&chunk));
            if seen.contains("first-chunk") {
                return true;
            }
        }
        false
    })
    .await
    .expect("delta relayed before the provider finished");
    assert!(got);
    tx.send(delta_frame("second")).unwrap();
    tx.send(completed_frame(5, 5)).unwrap();
    drop(tx);
    while let Some(Ok(chunk)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    let events = parse_sse(&seen);
    assert_eq!(events.last().unwrap().0, "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_before_first_content() {
    let h = Harness::with(Options {
        config: json!({"streaming": {"sse_ping_interval_seconds": 5}}),
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let tx = h.gw.push_channel();
    tx.send(created_frame()).unwrap();
    let resp = h
        .open_stream(&format!("/mini-chat/v1/chats/{chat}/messages:stream"), &a, json!({"content": "think"}))
        .await;
    let mut body = resp.into_body().into_data_stream();
    let mut raw = String::new();
    // Wait (bounded) for the idle ping, then let the provider produce content.
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(Ok(c)) = body.next().await {
            raw.push_str(&String::from_utf8_lossy(&c));
            if raw.contains("event: ping") {
                return;
            }
        }
    })
    .await
    .expect("ping before the first content");
    tx.send(delta_frame("late")).unwrap();
    tx.send(completed_frame(1, 1)).unwrap();
    drop(tx);
    while let Some(Ok(c)) = body.next().await {
        raw.push_str(&String::from_utf8_lossy(&c));
    }
    let events = parse_sse(&raw);
    let s = SseResp {
        status: StatusCode::OK,
        headers: http::HeaderMap::new(),
        events,
        raw,
        problem: serde_json::Value::Null,
    };
    let names: Vec<&str> = s.events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names[0], "stream_started");
    assert_eq!(names[1], "ping", "{names:?}");
    assert_eq!(s.events[1].1, json!({}));
    let first_delta = names.iter().position(|n| *n == "delta").unwrap();
    assert!(names[first_delta..].iter().all(|n| *n != "ping"), "no ping after content");
    assert_eq!(*names.last().unwrap(), "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_disconnect_cancels_turn_and_keeps_partial() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let tx = h.gw.push_channel();
    tx.send(created_frame()).unwrap();
    tx.send(delta_frame("partial answer")).unwrap();
    let rid = Uuid::new_v4();
    let resp = h
        .open_stream(
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            &a,
            json!({"content": "hi", "request_id": rid}),
        )
        .await;
    let mut body = resp.into_body().into_data_stream();
    let mut seen = String::new();
    while let Some(Ok(chunk)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&chunk));
        if seen.contains("partial answer") {
            break;
        }
    }
    drop(body);
    let state = h
        .eventually("turn cancelled", || async {
            let t = h.turn_row(rid).await;
            (t[0].as_deref() == Some("cancelled")).then_some(t)
        })
        .await;
    assert!(state[6].is_some(), "partial assistant message persisted");
    let st = h.get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), &a).await;
    assert_eq!(st.body["state"], "cancelled");
    assert!(st.body.get("assistant_message_id").is_some());
    let msgs = h.messages(&a, chat).await;
    assert_eq!(msgs.last().unwrap()["content"], "partial answer");
    drop(tx);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_without_content_persists_no_message() {
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
            json!({"content": "hi", "request_id": rid}),
        )
        .await;
    let mut body = resp.into_body().into_data_stream();
    let mut seen = String::new();
    while let Some(Ok(chunk)) = body.next().await {
        seen.push_str(&String::from_utf8_lossy(&chunk));
        if seen.contains("stream_started") {
            break;
        }
    }
    drop(body);
    h.eventually("turn cancelled", || async {
        (h.turn_row(rid).await[0].as_deref() == Some("cancelled")).then_some(())
    })
    .await;
    let st = h.get(&format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), &a).await;
    assert_eq!(st.body["state"], "cancelled");
    assert!(st.body.get("assistant_message_id").is_none(), "{}", st.text);
    assert_eq!(h.messages(&a, chat).await.len(), 1, "only the user message");
    drop(tx);
}

#[tokio::test(flavor = "multi_thread")]
async fn reasoning_deltas_and_incomplete_response() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.gw.push(Reply::Sse(vec![
        created_frame(),
        frame("response.reasoning_summary_text.delta", json!({"delta": "thinking..."})),
        delta_frame("answer"),
        frame(
            "response.incomplete",
            json!({"response": {"id": "resp_x", "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 3, "output_tokens": 1000}}}),
        ),
    ]));
    let s = h.send(&a, chat, "hi").await;
    let deltas: Vec<&Value> = s.events.iter().filter(|(n, _)| n == "delta").map(|(_, v)| v).collect();
    assert_eq!(deltas[0]["type"], "reasoning");
    assert_eq!(deltas[1]["type"], "text");
    assert_eq!(s.names().last(), Some(&"done"), "an incomplete response completes the turn");
    let msgs = h.messages(&a, chat).await;
    assert_eq!(msgs[1]["content"], "answer", "reasoning is not persisted as content");
}
