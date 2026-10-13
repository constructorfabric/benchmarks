//! Principles & Constraints.

use serde_json::json;

use crate::common::*;

/// Tenant and owner isolation enforced on every resource.
#[tokio::test]
async fn tenant_and_owner_isolation_on_every_resource() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let r = h.send_message(U1, chat, json!({"content": "hello"})).await;
    let rid = r.event("stream_started").unwrap()["request_id"].as_str().unwrap().to_owned();
    let up = h.upload(U1, chat, "a.txt", "text/plain", b"doc").await;
    let att = up.json()["id"].as_str().unwrap().to_owned();
    let msgs = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json();
    let asst = msgs["items"][1]["id"].as_str().unwrap().to_owned();

    for other in [U2, U3] {
        for (m, p, body) in [
            ("GET", format!("/chats/{chat}"), None),
            ("PATCH", format!("/chats/{chat}"), Some(json!({"title": "x"}))),
            ("GET", format!("/chats/{chat}/messages"), None),
            ("POST", format!("/chats/{chat}/messages:stream"), Some(json!({"content": "hi"}))),
            ("GET", format!("/chats/{chat}/attachments/{att}"), None),
            ("DELETE", format!("/chats/{chat}/attachments/{att}"), None),
            ("GET", format!("/chats/{chat}/turns/{rid}"), None),
            ("POST", format!("/chats/{chat}/turns/{rid}/retry"), None),
            ("DELETE", format!("/chats/{chat}/turns/{rid}"), None),
            ("PUT", format!("/chats/{chat}/messages/{asst}/reaction"), Some(json!({"reaction": "like"}))),
            ("DELETE", format!("/chats/{chat}"), None),
        ] {
            let r = h.call(other, m, &p, body).await;
            assert_eq!(r.status, 404, "{m} {p} as {other:?}: {}", r.text());
            assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
        }
        let up = h.upload(other, chat, "b.txt", "text/plain", b"x").await;
        assert_eq!(up.status, 404);
        let list = h.call(other, "GET", "/chats", None).await.json();
        assert!(list["items"].as_array().unwrap().iter().all(|c| c["id"] != chat.to_string()));
    }
    // The owner still sees everything unchanged.
    assert_eq!(h.call(U1, "GET", &format!("/chats/{chat}"), None).await.status, 200);
}

/// Context window budget enforced for the input message and the assembled request.
#[tokio::test]
async fn context_budget_for_message_and_assembled_request() {
    let h = Harness::new().await;
    // Message alone above max_input_tokens of the tiny model -> INPUT_TOO_LONG, no provider call.
    let chat = h.create_chat(U1, Some("tiny")).await;
    let r = h.send_message(U1, chat, json!({"content": "x".repeat(20_000)})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(r.reason(), "INPUT_TOO_LONG");
    assert_eq!(r.json()["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.out_of_range.v1~");
    assert!(h.provider.chat_requests().is_empty());

    // Mandatory context above the budget -> CONTEXT_BUDGET_EXCEEDED.
    let catalog = json!([model_entry("small", "standard", 1_000_000, 1_000_000, json!({
        "context_window": 2000, "max_output_tokens": 1000, "max_input_tokens": 0,
        "system_prompt": "s".repeat(4000)}))]);
    let h2 = Harness::with(Options { catalog, ..Options::default() }).await;
    let chat = h2.create_chat(U1, Some("small")).await;
    let r = h2.send_message(U1, chat, json!({"content": "hello"})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(r.reason(), "CONTEXT_BUDGET_EXCEEDED");
    assert!(h2.provider.chat_requests().is_empty());

    // Full assembled request: old history is truncated by whole turns to fit.
    let chat = h.create_chat(U1, Some("tiny")).await;
    for i in 0..4 {
        let r = h.send_message(U1, chat, json!({"content": format!("turn{i} {}", "w".repeat(3000))})).await;
        assert_eq!(r.status, 200);
    }
    let last = h.provider.chat_requests().pop().unwrap();
    let input = last["input"].as_array().unwrap();
    assert_eq!(input.last().unwrap()["content"][0]["text"].as_str().unwrap().split(' ').next(), Some("turn3"));
    assert!(input.len() < 7, "history must be truncated: {}", input.len());
    assert_eq!(input[0]["role"], "user", "kept history never starts with an answer");
    assert!(!input.iter().any(|m| m["content"][0]["text"].as_str().unwrap_or_default().starts_with("turn0")));
}

/// Streaming responses are never buffered before relaying.
#[tokio::test]
async fn streaming_is_not_buffered() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let (tx, _dropped) = h.provider.push_channel();
    let (status, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "hi"}))).await;
    assert_eq!(status, 200);
    tx.send(frame("response.output_text.delta", &json!({"delta": "first"})).into()).unwrap();
    let mut buf = String::new();
    // The first delta arrives while the provider has not finished.
    assert!(read_until(&mut body, &mut buf, |n, d| n == "delta" && d["content"] == "first").await);
    assert!(!buf.contains("event: done"));
    tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into()).unwrap();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "done").await);
}

/// A chat's model is immutable once set.
#[tokio::test]
async fn chat_model_is_immutable() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, Some("standard-1")).await;
    let r = h.call(U1, "PATCH", &format!("/chats/{chat}"), Some(json!({"title": "Renamed", "model": "premium-1"}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["model"], "standard-1");
    assert_eq!(r.json()["title"], "Renamed");
    let r = h.send_message(U1, chat, json!({"content": "hi"})).await;
    assert_eq!(r.event("done").unwrap()["selected_model"], "standard-1");
    let g = h.call(U1, "GET", &format!("/chats/{chat}"), None).await;
    assert_eq!(g.json()["model"], "standard-1");
}

/// Quota is checked before any outbound provider call.
#[tokio::test]
async fn quota_checked_before_outbound() {
    let h = Harness::with(Options { standard_limits: (1000, 1_000_000), ..Options::default() }).await;
    let chat = h.create_chat(U1, Some("standard-1")).await;
    let r = h.send_message(U1, chat, json!({"content": "hi"})).await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(r.json()["context"]["violations"][0]["description"], "quota_exceeded");
    assert!(h.provider.chat_requests().is_empty(), "no provider call on quota rejection");
    assert!(h.turns(chat).await.is_empty(), "no turn on preflight rejection");
}
