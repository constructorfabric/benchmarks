//! Streaming: send message and the SSE event contract.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use crate::common::*;

/// The send-message endpoint streams a response correlated by a request id.
#[tokio::test]
async fn send_message_streams_correlated_by_request_id() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let rid = Uuid::new_v4();
    let r = h.send_message(U1, chat, json!({"content": "hello", "request_id": rid})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let names = r.event_names();
    assert_eq!(names.first().map(String::as_str), Some("stream_started"));
    assert_eq!(names.last().map(String::as_str), Some("done"));
    let started = r.event("stream_started").unwrap();
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["is_new_turn"], true);
    let turn = h.turn(chat, rid).await;
    assert_eq!(started["message_id"], turn.assistant_message_id.unwrap().to_string());
    let deltas: String = r
        .events()
        .into_iter()
        .filter(|(n, _)| n == "delta")
        .map(|(_, d)| d["content"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(deltas, "Hello world");

    // Without a request id the server generates one.
    let r = h.send_message(U1, chat, json!({"content": "again"})).await;
    let generated = r.event("stream_started").unwrap()["request_id"].as_str().unwrap().to_owned();
    let generated = Uuid::parse_str(&generated).unwrap();
    assert_ne!(generated, rid);
    assert_eq!(h.turn(chat, generated).await.state, "completed");
    // The outbound request carries the provider model id and streaming flag.
    let req = h.provider.chat_requests().pop().unwrap();
    assert_eq!(req["model"], "premium-1-provider");
    assert_eq!(req["stream"], true);
}

/// Preflight validation runs before any provider call and creates no turn.
#[tokio::test]
async fn preflight_validation_precedes_provider_call() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let other_chat = h.create_chat(U1, None).await;
    let foreign = h.upload(U1, other_chat, "o.txt", "text/plain", b"other").await.json()["id"].as_str().unwrap().to_owned();
    let mine = h.upload(U1, chat, "m.txt", "text/plain", b"mine").await.json()["id"].as_str().unwrap().to_owned();
    let cases = vec![
        (json!({"content": ""}), 400, "EMPTY_CONTENT"),
        (json!({"content": "   \n\t"}), 400, "EMPTY_CONTENT"),
        (json!({"content": "x", "attachment_ids": [Uuid::new_v4()]}), 400, "invalid_attachment"),
        (json!({"content": "x", "attachment_ids": [foreign]}), 400, "invalid_attachment"),
        (json!({"content": "x", "attachment_ids": [mine, mine]}), 400, "invalid_attachment"),
        (json!({"content": "x".repeat(500_000)}), 400, "INPUT_TOO_LONG"),
    ];
    for (body, status, reason) in cases {
        let r = h.send_message(U1, chat, body).await;
        assert_eq!(r.status, status, "{}", r.text());
        assert_eq!(r.reason(), reason);
        assert!(r.headers["content-type"].to_str().unwrap().contains("json"), "pre-stream errors are JSON");
    }
    // Schema errors.
    let r = h.send_message(U1, chat, json!({"request_id": Uuid::new_v4()})).await;
    assert_eq!(r.status, 422);
    let r = h.send_message(U1, chat, json!({"content": "x", "request_id": "not-a-uuid"})).await;
    assert_eq!(r.status, 422);
    let r = h.send_message(U1, Uuid::new_v4(), json!({"content": "x"})).await;
    assert_eq!(r.status, 404);
    assert!(h.provider.chat_requests().is_empty());
    assert!(h.turns(chat).await.is_empty());
    assert!(h.messages(chat).await.is_empty());
    assert!(h.quota_rows(USER_1).await.iter().all(|q| q.reserved_credits_micro == 0 && q.spent_credits_micro == 0));
}

/// Assistant message and usage are persisted once the stream completes.
#[tokio::test]
async fn assistant_message_and_usage_persisted_on_completion() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, Some("standard-1")).await;
    h.provider.push(completed(&["Persisted", " answer"], 100, 20));
    let rid = Uuid::new_v4();
    let r = h.send_message(U1, chat, json!({"content": "q", "request_id": rid})).await;
    assert_eq!(r.event("done").unwrap()["usage"], json!({"input_tokens": 100, "output_tokens": 20}));
    let turn = h.turn(chat, rid).await;
    assert_eq!(turn.state, "completed");
    assert!(turn.completed_at.is_some());
    let msgs = h.messages(chat).await;
    assert_eq!(msgs.len(), 2);
    let asst = &msgs[1];
    assert_eq!(asst.role, "assistant");
    assert_eq!(asst.content, "Persisted answer");
    assert_eq!(asst.input_tokens, 100);
    assert_eq!(asst.output_tokens, 20);
    assert_eq!(asst.model.as_deref(), Some("standard-1"));
    assert_eq!(Some(asst.id), turn.assistant_message_id);
    // 100 * 1.0 + 20 * 3.0 = 160 credits_micro.
    let q = h.quota(USER_1, "total", "daily").await.unwrap();
    assert_eq!(q.spent_credits_micro, 160);
    assert_eq!(q.reserved_credits_micro, 0);
    assert_eq!(q.input_tokens, 100);
    assert_eq!(q.output_tokens, 20);
    assert_eq!(q.calls, 1);
    let published = h.wait_published(1).await;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].actual_credits_micro, 160);
}

/// Full event contract: start, delta, tool activity, citations, done, in order; ping before content.
#[tokio::test]
async fn sse_event_contract_and_ordering() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    h.provider.push(Script::Sse(vec![
        ("response.created".into(), json!({"response": {"id": "resp_x1234567890"}})),
        ("response.output_text.delta".into(), json!({"delta": "Searching. "})),
        ("response.web_search_call.searching".into(), json!({})),
        ("response.web_search_call.completed".into(), json!({})),
        ("response.output_text.delta".into(), json!({"delta": "Rust 1.80 is out."})),
        ("response.output_text.annotation.added".into(), json!({"annotation": {"type": "url_citation",
            "url": "https://blog.rust-lang.org/x", "title": "Rust Blog", "start_index": 11, "end_index": 20}})),
        ("response.completed".into(), json!({"response": {"id": "resp_x1234567890", "usage": {"input_tokens": 50, "output_tokens": 9}}})),
    ]));
    let r = h.send_message(U1, chat, json!({"content": "news?", "web_search": {"enabled": true}})).await;
    let names = r.event_names();
    let pos = |n: &str| names.iter().position(|x| x == n).unwrap_or_else(|| panic!("missing {n} in {names:?}"));
    assert_eq!(pos("stream_started"), 0);
    assert!(pos("delta") < pos("tool"));
    assert!(pos("tool") < pos("citations"));
    assert!(pos("citations") < pos("done"));
    assert_eq!(names.last().unwrap(), "done");
    let evs = r.events();
    let tools: Vec<_> = evs.iter().filter(|(n, _)| n == "tool").map(|(_, d)| d.clone()).collect();
    assert_eq!(tools[0]["phase"], "start");
    assert_eq!(tools[0]["name"], "web_search");
    assert_eq!(tools[1]["phase"], "done");
    let deltas: Vec<_> = evs.iter().filter(|(n, _)| n == "delta").map(|(_, d)| d.clone()).collect();
    assert!(deltas.iter().all(|d| d["type"] == "text"), "the Responses adapter emits text deltas only");
    let cit = r.event("citations").unwrap();
    assert_eq!(cit["items"][0]["source"], "web");
    assert_eq!(cit["items"][0]["url"], "https://blog.rust-lang.org/x");
    assert_eq!(cit["items"][0]["title"], "Rust Blog");
    assert_eq!(cit["items"][0]["span"], json!({"start": 11, "end": 20}));
    assert_eq!(h.messages(chat).await[1].content, "Searching. Rust 1.80 is out.");

    // Keepalive ping is emitted while waiting for the first content.
    let (tx, _d) = h.provider.push_channel();
    let (status, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "slow"}))).await;
    assert_eq!(status, 200);
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    tokio::time::sleep(Duration::from_millis(5300)).await;
    assert!(read_until(&mut body, &mut buf, |n, _| n == "ping").await, "{buf}");
    tx.send(frame("response.output_text.delta", &json!({"delta": "late"})).into()).unwrap();
    tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into()).unwrap();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "done").await);
    let order: Vec<String> = parse_sse(&buf).into_iter().map(|(n, _)| n).collect();
    let ping = order.iter().position(|n| n == "ping").unwrap();
    let delta = order.iter().position(|n| n == "delta").unwrap();
    assert!(ping < delta, "pings precede content: {order:?}");
}

/// Done exposes usage and quota/downgrade outcome without internal identifiers.
#[tokio::test]
async fn done_exposes_usage_and_quota_outcome_without_internal_ids() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let r = h.send_message(U1, chat, json!({"content": "hi"})).await;
    let done = r.event("done").unwrap();
    assert_eq!(done["usage"]["input_tokens"], 10);
    assert_eq!(done["usage"]["output_tokens"], 5);
    assert_eq!(done["effective_model"], "premium-1");
    assert_eq!(done["selected_model"], "premium-1");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_from").is_none());
    let warnings = done["quota_warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 4, "premium/standard x daily/monthly");
    for w in warnings {
        assert!(w["remaining_percentage"].as_i64().unwrap() <= 100);
        assert_eq!(w["warning"], false);
        assert_eq!(w["exhausted"], false);
    }
    let text = r.text();
    for leak in ["resp_", "premium-1-provider", "tenant_id", "user_id", "credits_micro", "file-"] {
        assert!(!text.contains(leak), "SSE leaks {leak}: {text}");
    }
}

/// The error event is terminal and carries a sanitized message.
#[tokio::test]
async fn error_event_is_terminal_and_sanitized() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    h.provider.push(Script::Http(
        500,
        json!({"error": {"message": "upstream https://api.openai.com/v1/responses failed for file-abcdefghijkl1234 using sk-ABCDEFGHIJKLMNOP"}}),
        vec![],
    ));
    let rid = Uuid::new_v4();
    let r = h.send_message(U1, chat, json!({"content": "hi", "request_id": rid})).await;
    assert_eq!(r.status, 200, "provider errors after stream start are SSE events");
    let names = r.event_names();
    assert_eq!(names.last().unwrap(), "error");
    assert!(!names.contains(&"done".to_owned()));
    assert_eq!(names.iter().filter(|n| *n == "error").count(), 1);
    let err = r.event("error").unwrap();
    assert_eq!(err["code"], "provider_error");
    let msg = err["message"].as_str().unwrap();
    assert!(!msg.contains("api.openai.com") && !msg.contains("file-abcdefghijkl1234") && !msg.contains("sk-ABCD"), "{msg}");
    assert!(msg.contains("[url]") && msg.contains("[provider_id]") && msg.contains("[credential]"), "{msg}");
    let turn = h.turn(chat, rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));

    // Rate limiting keeps the retry hint.
    h.provider.push(Script::Http(429, json!({"error": {"message": "slow down"}}), vec![("retry-after".into(), "7".into())]));
    let r = h.send_message(U1, chat, json!({"content": "hi"})).await;
    let err = r.event("error").unwrap();
    assert_eq!(err["code"], "rate_limited");
    assert!(err["message"].as_str().unwrap().contains("retry in 7s"));

    // A provider stream that ends without a terminal frame fails the turn.
    h.provider.push(Script::Sse(vec![("response.output_text.delta".into(), json!({"delta": "half"}))]));
    let r = h.send_message(U1, chat, json!({"content": "hi"})).await;
    assert_eq!(r.event_names().last().unwrap(), "error");
    assert_eq!(r.event("error").unwrap()["code"], "provider_error");
    // Mid-stream provider failure.
    h.provider.push(Script::Sse(vec![
        ("response.output_text.delta".into(), json!({"delta": "part"})),
        ("response.failed".into(), json!({"response": {"error": {"message": "model resp_abc123def crashed"}}})),
    ]));
    let r = h.send_message(U1, chat, json!({"content": "hi"})).await;
    let err = r.event("error").unwrap();
    assert_eq!(err["code"], "provider_error");
    assert!(!err["message"].as_str().unwrap().contains("resp_abc123def"));
}
