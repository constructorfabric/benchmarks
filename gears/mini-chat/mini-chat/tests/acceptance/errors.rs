//! Error mapping & sanitization.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::*;
use mini_chat::infra::llm::TransportError;

fn assert_problem(r: &Resp, status: u16, category: &str) -> Value {
    assert_eq!(r.status, status, "{}", r.text());
    let ct = r.headers["content-type"].to_str().unwrap();
    assert!(ct.starts_with("application/problem+json"), "content-type {ct}");
    let p = r.json();
    assert_eq!(p["type"], format!("gts://gts.cf.core.errors.err.v1~cf.core.err.{category}.v1~"), "{p}");
    assert_eq!(p["status"], status);
    assert!(p["title"].is_string());
    assert!(p["detail"].is_string());
    assert!(p["context"].is_object());
    p
}

/// All errors map to the canonical error contract, consistently across REST and streaming.
#[tokio::test]
async fn canonical_error_contract_rest_and_streaming() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let missing = Uuid::new_v4();
    // not_found
    let p = assert_problem(&h.call(U1, "GET", &format!("/chats/{missing}"), None).await, 404, "not_found");
    assert_eq!(p["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
    assert!(!p.to_string().contains(&missing.to_string()) || p["context"]["resource_name"].is_string());
    // invalid_argument with field violations
    let p = assert_problem(&h.call(U1, "POST", "/chats", Some(json!({"model": "nope"}))).await, 400, "invalid_argument");
    assert_eq!(p["context"]["field_violations"][0]["field"], "model");
    // out_of_range
    assert_problem(&h.call(U1, "POST", "/chats", Some(json!({"title": ""}))).await, 400, "invalid_argument");
    // path parameter errors
    let p = assert_problem(&h.call(U1, "GET", "/chats/not-a-uuid", None).await, 400, "invalid_argument");
    assert!(p.to_string().contains("invalid_path_params") || p["context"]["field_violations"].is_array(), "{p}");
    // body errors: malformed JSON and missing content type
    let req = http::Request::builder()
        .method("POST")
        .uri("/mini-chat/v1/chats")
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{not json"))
        .unwrap();
    let r = h.send(req).await;
    assert!(r.status == 400 || r.status == 422, "{}", r.status);
    assert!(r.headers["content-type"].to_str().unwrap().starts_with("application/problem+json"));
    let req = http::Request::builder()
        .method("POST")
        .uri("/mini-chat/v1/chats")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    let r = h.send(req).await;
    assert_eq!(r.status, 415);
    assert_eq!(r.reason(), "missing_json_content_type");
    // aborted
    let (tx, _d) = h.provider.push_channel();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "a"}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    let p = assert_problem(&h.send_message(U1, chat, json!({"content": "b"})).await, 409, "aborted");
    assert_eq!(p["context"]["reason"], "turn_already_running");
    let _ = tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into());
    assert!(read_until(&mut body, &mut buf, |n, _| n == "done").await);
    // failed_precondition
    let msgs = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json();
    let user_msg = msgs["items"][0]["id"].as_str().unwrap().to_owned();
    let p = assert_problem(
        &h.call(U1, "PUT", &format!("/chats/{chat}/messages/{user_msg}/reaction"), Some(json!({"reaction": "like"}))).await,
        400,
        "failed_precondition",
    );
    assert_eq!(p["context"]["violations"][0]["type"], "STATE");
    // resource_exhausted (quota) — pre-stream JSON, not SSE.
    let hq = Harness::with(Options { standard_limits: (1, 1), ..Options::default() }).await;
    let c = hq.create_chat(U1, None).await;
    let p = assert_problem(&hq.send_message(U1, c, json!({"content": "x"})).await, 429, "resource_exhausted");
    assert_eq!(p["context"]["violations"][0]["subject"], "tokens");
    // out_of_range on streaming preflight
    let tiny = h.create_chat(U1, Some("tiny")).await;
    let p = assert_problem(&h.send_message(U1, tiny, json!({"content": "x".repeat(20_000)})).await, 400, "out_of_range");
    assert_eq!(p["context"]["field_violations"][0]["reason"], "INPUT_TOO_LONG");
    // Problems never leak internal identifiers.
    let r = h.call(U2, "GET", &format!("/chats/{chat}"), None).await;
    assert!(!r.text().contains(&USER_1.to_string()) && !r.text().contains(&TENANT_A.to_string()));

    // Streaming: failures after the stream started are terminal SSE error events with stable codes.
    for (script, code) in [
        (Script::Http(500, json!({"error": {"message": "x"}}), vec![]), "provider_error"),
        (Script::Http(429, json!({"error": {"message": "x"}}), vec![]), "rate_limited"),
        (Script::Gateway(TransportError::Timeout("t".into())), "provider_timeout"),
        (Script::Gateway(TransportError::Gateway("upstream https://10.0.0.1:443 refused".into())), "provider_error"),
    ] {
        h.provider.push(script);
        let rid = Uuid::new_v4();
        let r = h.send_message(U1, chat, json!({"content": "x", "request_id": rid})).await;
        assert_eq!(r.status, 200);
        let names = r.event_names();
        assert_eq!(names.first().unwrap(), "stream_started");
        assert_eq!(names.last().unwrap(), "error");
        let e = r.event("error").unwrap();
        assert_eq!(e["code"], code);
        assert!(!e["message"].as_str().unwrap().contains("10.0.0.1"), "{e}");
        let t = h.call(U1, "GET", &format!("/chats/{chat}/turns/{rid}"), None).await.json();
        assert_eq!(t["state"], "error");
        assert_eq!(t["error_code"], code, "turn status uses the same code as the SSE error");
    }
}

/// Provider-originated error details are sanitized before reaching the client.
#[tokio::test]
async fn provider_error_details_sanitized() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let raw = "Invalid file file-AbCdEfGhIjKlMnOp in vs_ZyXwVuTsRqPoNm for resp_9f8e7d6c, see https://api.openai.com/v1/files (key sk-proj-abcdefghijklmnop, Bearer eyJhbGciOi.payload)";
    let leaks = ["file-AbCdEfGhIjKlMnOp", "vs_ZyXwVuTsRqPoNm", "resp_9f8e7d6c", "api.openai.com", "sk-proj-abcdefghijklmnop", "eyJhbGciOi"];
    // HTTP error body.
    h.provider.push(Script::Http(400, json!({"error": {"message": raw}}), vec![]));
    let rid = Uuid::new_v4();
    let r = h.send_message(U1, chat, json!({"content": "x", "request_id": rid})).await;
    let msg = r.event("error").unwrap()["message"].as_str().unwrap().to_owned();
    for l in leaks {
        assert!(!msg.contains(l), "SSE error leaks {l}: {msg}");
    }
    assert!(msg.starts_with("Invalid file [provider_id]"), "{msg}");
    // Persisted detail is sanitized as well.
    let t = h.turn(chat, rid).await;
    let detail = t.error_detail.unwrap_or_default();
    for l in leaks {
        assert!(!detail.contains(l), "stored detail leaks {l}");
    }
    // Mid-stream error frame.
    h.provider.push(Script::Sse(vec![
        ("response.output_text.delta".into(), json!({"delta": "x"})),
        ("error".into(), json!({"error": {"message": raw}})),
    ]));
    let r = h.send_message(U1, chat, json!({"content": "y"})).await;
    let msg = r.event("error").unwrap()["message"].as_str().unwrap().to_owned();
    for l in leaks {
        assert!(!msg.contains(l), "mid-stream error leaks {l}");
    }
    // Turn status and audit never carry the raw message.
    let text = h.call(U1, "GET", &format!("/chats/{chat}/turns/{rid}"), None).await.text();
    for l in leaks {
        assert!(!text.contains(l));
    }
    let audit = h.wait_audit(2).await;
    let dump = serde_json::to_string(&audit).unwrap();
    for l in ["sk-proj-abcdefghijklmnop", "eyJhbGciOi"] {
        assert!(!dump.contains(l), "audit leaks {l}");
    }
}
