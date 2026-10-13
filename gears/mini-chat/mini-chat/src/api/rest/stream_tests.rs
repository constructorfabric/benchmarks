//! Router tests: messages API, send-message streaming, SSE contract, error
//! mapping and sanitization (acceptance: Messages API, Streaming, SSE Event
//! Contract, Error Mapping, Principles "no buffering" / "context budget").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::infra::llm::sse_parser::SseParser;
use crate::test_support::{
    FakeResponse, TestEnv, json_resp, live_provider, next_event, sse_chunk, sse_resp, user_a,
};

fn hex(id: &str) -> String {
    id.replace('-', "")
}

#[tokio::test]
async fn stream_contract_and_persistence() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let rid = Uuid::new_v4();
    let r = env.stream(&user_a(), &chat, json!({"content": "Hi there", "request_id": rid})).await;
    assert_eq!(r.status, 200);
    assert!(r.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    assert_eq!(r.event_names(), vec!["stream_started", "delta", "delta", "done"]);
    let started = r.event("stream_started").unwrap();
    assert_eq!(started["request_id"], rid.to_string());
    assert_eq!(started["is_new_turn"], true);
    let msg_id = started["message_id"].as_str().unwrap().to_owned();
    let deltas: String = r
        .events()
        .iter()
        .filter(|(n, _)| n == "delta")
        .map(|(_, d)| {
            assert_eq!(d["type"], "text");
            d["content"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(deltas, "Hello world");
    let done = r.event("done").unwrap();
    assert_eq!(done["usage"], json!({"input_tokens": 12, "output_tokens": 5}));
    assert_eq!(done["effective_model"], "gpt-4.1");
    assert_eq!(done["selected_model"], "gpt-4.1");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_from").is_none());
    assert_eq!(done["quota_warnings"].as_array().unwrap().len(), 4);
    let text = r.text();
    for leak in ["resp_test123", "turn_id", "provider_response_id"] {
        assert!(!text.contains(leak), "SSE must not leak {leak}");
    }
    // persisted messages + turn
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    let items = msgs["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[0]["request_id"], rid.to_string());
    assert!(items[0].get("model").is_none());
    assert_eq!(items[1]["id"], msg_id.as_str());
    assert_eq!(items[1]["role"], "assistant");
    assert_eq!(items[1]["content"], "Hello world");
    assert_eq!(items[1]["model"], "gpt-4.1");
    assert_eq!(items[1]["input_tokens"], 12);
    assert_eq!(items[1]["request_id"], rid.to_string());
    for i in items {
        assert_eq!(i["attachments"], json!([]));
        assert!(i.get("my_reaction").unwrap().is_null());
    }
    let turn = env
        .sql_rows(&format!(
            "SELECT state, reserve_tokens, max_output_tokens_applied, reserved_credits_micro, policy_version_applied, effective_model, minimal_generation_floor_applied, completed_at, provider_response_id FROM chat_turns WHERE request_id = x'{}'",
            rid.simple()
        ))
        .await;
    let t = &turn[0];
    assert_eq!(t.try_get_by_index::<String>(0).unwrap(), "completed");
    assert!(t.try_get_by_index::<i64>(1).unwrap() > 32768);
    assert_eq!(t.try_get_by_index::<i64>(2).unwrap(), 32768);
    assert!(t.try_get_by_index::<i64>(3).unwrap() > 0);
    assert_eq!(t.try_get_by_index::<i64>(4).unwrap(), 1);
    assert_eq!(t.try_get_by_index::<String>(5).unwrap(), "gpt-4.1");
    assert_eq!(t.try_get_by_index::<i64>(6).unwrap(), 50);
    assert!(t.try_get_by_index::<Option<String>>(7).unwrap().is_some());
    assert_eq!(t.try_get_by_index::<String>(8).unwrap(), "resp_test123");
    let chat_v = env.get(&format!("/mini-chat/v1/chats/{chat}")).await.json();
    assert_eq!(chat_v["message_count"], 2);
    // provider request shape
    let req = env.proxy.chat_requests()[0].json();
    assert_eq!(req["model"], "gpt-4.1");
    assert_eq!(req["stream"], true);
    assert_eq!(req["max_output_tokens"], 32768);
    assert!(req["instructions"].as_str().unwrap().starts_with("You are a helpful assistant."));
    assert_eq!(req["input"].as_array().unwrap().last().unwrap()["content"], "Hi there");
    assert_eq!(req["user"].as_str().unwrap().len(), 64);
    assert_eq!(req["metadata"]["chat_id"], chat.as_str());
    assert_eq!(req["metadata"]["request_type"], "chat");
    assert_eq!(req["metadata"]["feature"], "none");
    assert!(req.get("tools").is_none(), "no tools without documents or web search");
    assert!(env.proxy.chat_requests()[0].uri.starts_with("/127.0.0.1/v1/responses"));
}

#[tokio::test]
async fn server_generates_request_id_and_history_is_sent() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let r1 = env.send_msg(&chat, "first").await;
    let rid1 = r1.event("stream_started").unwrap()["request_id"].as_str().unwrap().to_owned();
    assert_eq!(Uuid::parse_str(&rid1).unwrap().get_version_num(), 4);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let r2 = env.send_msg(&chat, "second").await;
    assert_eq!(r2.status, 200);
    let req = env.proxy.chat_requests()[1].json();
    let input = req["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(input[0], json!({"role": "user", "content": "first"}));
    assert_eq!(input[1], json!({"role": "assistant", "content": "Hello world"}));
    assert_eq!(input[2]["content"], "second");
    // chronological message order, count 4
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    let roles: Vec<&str> = msgs["items"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
    // filtering, ordering, pagination on messages
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/messages?$filter=role%20eq%20'assistant'")).await;
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 2);
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/messages?$orderby=created_at%20desc")).await;
    assert_eq!(r.json()["items"][0]["role"], "assistant");
    assert_eq!(r.json()["items"][0]["content"], "Hello world");
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/messages?limit=3")).await;
    let v = r.json();
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    let cur = v["page_info"]["next_cursor"].as_str().unwrap().to_owned();
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/messages?limit=3&cursor={cur}")).await;
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 1, "{}", r.text());
    assert_eq!(r.json()["items"][0]["role"], "assistant");
    let id = msgs["items"][1]["id"].as_str().unwrap();
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/messages?$filter=id%20eq%20{id}")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 1, "{}", r.text());
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/messages?$filter=id%20eq%20'{id}'")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["items"][0]["id"], id);
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 1);
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/messages?$filter=nope%20eq%201")).await;
    r.assert_problem(400, "INVALID_FILTER");
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/messages?limit=0")).await;
    r.assert_problem(400, "INVALID_LIMIT");
}

#[tokio::test]
async fn deltas_are_relayed_without_buffering() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let tx = live_provider(&env.proxy);
    let (status, mut body) = env
        .open(&user_a(), "POST", &format!("/mini-chat/v1/chats/{chat}/messages:stream"), json!({"content": "go"}))
        .await;
    assert_eq!(status, 200);
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    for word in ["one", "two", "three"] {
        tx.send(sse_chunk("response.output_text.delta", &json!({"item_id": "m", "content_index": 0, "delta": word})))
            .await
            .unwrap();
        let (name, data) = next_event(&mut body, &mut p, &mut q).await.unwrap();
        assert_eq!(name, "delta");
        assert_eq!(data["content"], word, "each delta is relayed before the next arrives");
    }
    tx.send(sse_chunk("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 3}}})))
        .await
        .unwrap();
    let (name, _) = next_event(&mut body, &mut p, &mut q).await.unwrap();
    assert_eq!(name, "done");
}

#[tokio::test]
async fn ping_before_first_content() {
    let env = TestEnv::with(crate::test_support::EnvOptions {
        config: json!({"streaming": {"sse_ping_interval_seconds": 5}}),
        ..Default::default()
    })
    .await;
    let chat = env.chat(None).await;
    let tx = live_provider(&env.proxy);
    let (_, mut body) = env
        .open(&user_a(), "POST", &format!("/mini-chat/v1/chats/{chat}/messages:stream"), json!({"content": "go"}))
        .await;
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    let (name, data) = tokio::time::timeout(std::time::Duration::from_secs(8), next_event(&mut body, &mut p, &mut q))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(name, "ping");
    assert_eq!(data, json!({}));
    tx.send(sse_chunk("response.output_text.delta", &json!({"delta": "x"}))).await.unwrap();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "delta");
    tx.send(sse_chunk("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})))
        .await
        .unwrap();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "done");
}

fn turn_state(rows: &[sea_orm::QueryResult]) -> (String, Option<String>, Option<Vec<u8>>) {
    let r = &rows[0];
    (
        r.try_get_by_index::<String>(0).unwrap(),
        r.try_get_by_index::<Option<String>>(1).unwrap(),
        r.try_get_by_index::<Option<Vec<u8>>>(2).unwrap(),
    )
}

async fn last_turn(env: &TestEnv, chat: &str) -> (String, Option<String>, Option<Vec<u8>>) {
    turn_state(
        &env.sql_rows(&format!(
            "SELECT state, error_code, assistant_message_id FROM chat_turns WHERE chat_id = x'{}' ORDER BY started_at DESC LIMIT 1",
            hex(chat)
        ))
        .await,
    )
}

#[tokio::test]
async fn provider_errors_map_to_sanitized_sse_errors() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let cases: Vec<(FakeResponse, &str)> = vec![
        (
            json_resp(500, &json!({"error": {"message": "file-abcdefghijklmnop missing at https://x.example.com/v1 key sk-abcdefghijklmnop"}})),
            "provider_error",
        ),
        (
            FakeResponse::Raw(429, vec![("retry-after".into(), "7".into())], vec![bytes::Bytes::from("{}")]),
            "rate_limited",
        ),
        (FakeResponse::Gateway(crate::domain::error::ChatResource::deadline_exceeded("timeout").create()), "provider_timeout"),
        (
            FakeResponse::Raw(504, vec![("x-oagw-error-source".into(), "gateway".into())], vec![bytes::Bytes::from("{}")]),
            "provider_timeout",
        ),
        (
            sse_resp(&[
                ("response.output_text.delta".into(), json!({"delta": "par"})),
                ("response.failed".into(), json!({"response": {"error": {"message": "failed resp_abc123 vs_abcdefghijklmn"}}})),
            ]),
            "provider_error",
        ),
        (sse_resp(&[("error".into(), json!({"code": "x", "message": "bad thing"}))]), "provider_error"),
        (sse_resp(&[("response.output_text.delta".into(), json!({"delta": "cut"}))]), "provider_error"),
    ];
    for (resp, code) in cases {
        let slot = std::sync::Mutex::new(Some(resp));
        env.proxy.respond(move |r| if r.uri.contains("/responses") { slot.lock().unwrap().take() } else { None });
        let r = env.send_msg(&chat, "x").await;
        assert_eq!(r.status, 200);
        let names = r.event_names();
        assert_eq!(names.first().map(String::as_str), Some("stream_started"));
        assert_eq!(names.last().map(String::as_str), Some("error"), "{names:?}");
        let err = r.event("error").unwrap();
        assert_eq!(err["code"], code);
        let msg = err["message"].as_str().unwrap();
        for leak in ["file-abcdefghijklmnop", "https://", "sk-abcdefghij", "resp_abc123", "vs_abcdefghijklmn"] {
            assert!(!msg.contains(leak), "leaked {leak} in {msg}");
        }
        if code == "rate_limited" {
            assert!(msg.contains('7'), "{msg}");
        }
        let (state, ec, msg_id) = last_turn(&env, &chat).await;
        assert_eq!(state, "failed");
        assert_eq!(ec.as_deref(), Some(code));
        assert!(msg_id.is_none(), "failed turn has no assistant message");
    }
    // failed turns leave only user messages
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert!(msgs["items"].as_array().unwrap().iter().all(|m| m["role"] == "user"));
}

#[tokio::test]
async fn incomplete_completes_and_empty_answer_completes() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    env.proxy.respond(|r| {
        r.uri.contains("/responses").then(|| {
            sse_resp(&[
                ("response.output_text.delta".into(), json!({"delta": "trunc"})),
                ("response.incomplete".into(), json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 3, "output_tokens": 4}}})),
            ])
        })
    });
    let r = env.send_msg(&chat, "x").await;
    assert_eq!(r.event_names().last().unwrap(), "done");
    let (state, ec, _) = last_turn(&env, &chat).await;
    assert_eq!((state.as_str(), ec), ("completed", None));
    env.proxy.respond(|r| {
        r.uri.contains("/responses").then(|| {
            sse_resp(&[("response.completed".into(), json!({"response": {"usage": {"input_tokens": 3, "output_tokens": 0}}}))])
        })
    });
    let r = env.send_msg(&chat, "y").await;
    assert_eq!(r.event_names(), vec!["stream_started", "done"]);
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert_eq!(msgs["items"].as_array().unwrap().last().unwrap()["content"], "");
}

#[tokio::test]
async fn preflight_validation_happens_before_provider_call() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let r = env.send_msg(&chat, "   ").await;
    r.assert_problem(400, "EMPTY_CONTENT");
    let r = env.stream(&user_a(), &chat, json!({"content": "x", "attachment_ids": [Uuid::new_v4()]})).await;
    r.assert_problem(400, "invalid_attachment");
    let dup = Uuid::new_v4();
    let r = env.stream(&user_a(), &chat, json!({"content": "x", "attachment_ids": [dup, dup]})).await;
    r.assert_problem(400, "invalid_attachment");
    let r = env.stream(&user_a(), &chat, json!({"content": "x", "attachment_ids": ["nope"]})).await;
    r.assert_problem(422, "invalid_json_body");
    let r = env.stream(&user_a(), &Uuid::new_v4().to_string(), json!({"content": "x"})).await;
    r.assert_problem(404, "gts.cf.core.mini_chat.chat.v1~");
    // chat model removed from the catalog
    env.sql_exec(&format!("UPDATE chats SET model = 'ghost' WHERE id = x'{}'", hex(&chat))).await;
    let r = env.send_msg(&chat, "x").await;
    r.assert_problem(400, "INVALID_MODEL");
    assert!(env.proxy.chat_requests().is_empty(), "no provider call on preflight failure");
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_turns").await, 0);
    assert_eq!(env.count("SELECT COUNT(*) FROM messages").await, 0);
}

#[tokio::test]
async fn context_budget_is_enforced_for_message_and_full_request() {
    let env = TestEnv::new().await;
    let chat = env.chat(Some("tiny-ctx")).await;
    // message alone above max_input_tokens (3072)
    let r = env.send_msg(&chat, &"a".repeat(20_000)).await;
    r.assert_problem(400, "INPUT_TOO_LONG");
    // message fits max_input_tokens but not the assembled budget (system prompt + overhead)
    let r = env.send_msg(&chat, &"a".repeat(10_500)).await;
    r.assert_problem(400, "CONTEXT_BUDGET_EXCEEDED");
    assert!(env.proxy.chat_requests().is_empty());
    assert_eq!(env.count("SELECT COUNT(*) FROM quota_usage WHERE reserved_credits_micro <> 0").await, 0);
}

#[tokio::test]
async fn history_is_truncated_by_whole_turns_within_budget() {
    let env = TestEnv::new().await;
    let chat = env.chat(Some("tiny-ctx")).await;
    let long = "b".repeat(2_400);
    for _ in 0..4 {
        let r = env.send_msg(&chat, &long).await;
        assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    }
    let reqs = env.proxy.chat_requests();
    let last: Value = reqs.last().unwrap().json();
    let input = last["input"].as_array().unwrap();
    assert!(input.len() < 7, "older turns must be dropped: {}", input.len());
    assert_eq!(input[0]["role"], "user", "never starts with an answer");
}

#[tokio::test]
async fn function_tool_use_without_knowledge_search_is_unexpected_tool_use() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    env.proxy.respond(|r| {
        r.uri.contains("/responses").then(|| {
            sse_resp(&[
                ("response.output_text.delta".into(), json!({"delta": "let me check"})),
                (
                    "response.output_item.done".into(),
                    json!({"item": {"type": "function_call", "call_id": "call_x", "name": "get_weather", "arguments": "{}"}}),
                ),
                ("response.completed".into(), json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 5}}})),
            ])
        })
    });
    let r = env.send_msg(&chat, "weather?").await;
    assert_eq!(r.status, 200);
    let names = r.event_names();
    assert_eq!(names.last().map(String::as_str), Some("error"), "{names:?}");
    let err = r.event("error").unwrap();
    assert_eq!(err["code"], "unexpected_tool_use");
    assert!(!err["message"].as_str().unwrap().contains("get_weather"));
    let (state, ec, msg) = last_turn(&env, &chat).await;
    assert_eq!((state.as_str(), ec.as_deref()), ("failed", Some("unexpected_tool_use")));
    assert!(msg.is_none());
    env.eventually("usage", |e| e.usage_events().len() == 1).await;
    let u = &env.usage_events()[0];
    assert_eq!(u.billing_outcome, "failed");
    assert_eq!(u.settlement_method, "estimated", "provider was called: never released");
    // no search_knowledge tool is offered while the feature is off
    let req = env.proxy.chat_requests()[0].json();
    assert!(req.get("tools").is_none());
}
