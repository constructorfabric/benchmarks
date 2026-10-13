//! Router tests: knowledge search (`search_knowledge` function tool, agentic
//! loop, limits, mutual exclusion with `file_search`) — DESIGN §4 "Knowledge
//! Search" (acceptance: Context Assembly "tool availability and guidance").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};

use crate::test_support::{EnvOptions, Recorded, TestEnv, json_resp, sse_resp, user_a};

fn ks_env(max_calls: u32) -> EnvOptions {
    EnvOptions {
        config: json!({
            "providers": {"mock": {"api_version": "2025-04-01-preview"}},
            "knowledge_search": {"enabled": true, "vector_store_id": "vs_kb", "provider_id": "mock", "max_calls_per_message": max_calls, "top_k": 2, "max_chunk_chars": 5}
        }),
        ..Default::default()
    }
}

fn outputs_in(r: &Recorded) -> usize {
    r.json()["input"]
        .as_array()
        .map_or(0, |a| a.iter().filter(|i| i["type"] == "function_call_output").count())
}

fn call(id: &str, query: &str) -> Vec<(String, Value)> {
    vec![
        (
            "response.output_item.done".into(),
            json!({"item": {"type": "function_call", "call_id": id, "name": "search_knowledge", "arguments": json!({"query": query, "top_k": 9}).to_string()}}),
        ),
        ("response.completed".into(), json!({"response": {"usage": {"input_tokens": 100, "output_tokens": 10}}})),
    ]
}

fn answer() -> Vec<(String, Value)> {
    vec![
        ("response.output_text.delta".into(), json!({"delta": "From the handbook."})),
        ("response.completed".into(), json!({"response": {"usage": {"input_tokens": 7, "output_tokens": 3}}})),
    ]
}

fn install_search(env: &TestEnv) {
    env.proxy.respond(|r| {
        r.uri.contains("/vector_stores/vs_kb/search").then(|| {
            json_resp(
                200,
                &json!({"data": [
                    {"file_id": "file-kb1", "filename": "handbook.pdf", "score": 0.9, "content": [{"type": "text", "text": "VPN setup steps"}]},
                    {"file_id": "file-kb2", "filename": "faq.md", "score": 0.5, "content": [{"type": "text", "text": "More"}]},
                    {"file_id": "file-kb3", "filename": "extra.md", "score": 0.1, "content": [{"type": "text", "text": "Extra"}]}
                ]}),
            )
        })
    });
}

#[tokio::test]
async fn knowledge_search_loop_retrieves_and_answers() {
    let env = TestEnv::with(ks_env(3)).await;
    install_search(&env);
    env.proxy.respond(|r| {
        r.uri.contains("/responses").then(|| if outputs_in(r) == 0 { sse_resp(&call("call_1", "vpn")) } else { sse_resp(&answer()) })
    });
    let chat = env.chat(None).await;
    let r = env.send_msg(&chat, "how do I set up the VPN?").await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    assert_eq!(r.event("done").unwrap()["usage"], json!({"input_tokens": 7, "output_tokens": 3}), "final iteration usage");
    let reqs = env.proxy.chat_requests();
    assert_eq!(reqs.len(), 2);
    let first = reqs[0].json();
    let tools = first["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["type"] == "function" && t["name"] == "search_knowledge"));
    assert!(!tools.iter().any(|t| t["type"] == "file_search"));
    let guard = &env.svc.cfg.knowledge_search.guard;
    assert!(first["instructions"].as_str().unwrap().contains(guard.as_str()), "guard appended");
    assert_eq!(first["metadata"]["feature"], "none", "function tools are not a feature label");
    // retrieval through OAGW with top_k capped and chunks trimmed
    let search = env.proxy.requests_to("/openai/vector_stores/vs_kb/search");
    assert_eq!(search.len(), 1);
    assert!(search[0].uri.starts_with("/127.0.0.1/openai/vector_stores/vs_kb/search?api-version=2025-04-01-preview"), "{}", search[0].uri);
    assert_eq!(search[0].json(), json!({"query": "vpn", "max_num_results": 2}));
    let second = reqs[1].json();
    let input = second["input"].as_array().unwrap();
    let fc = input.iter().find(|i| i["type"] == "function_call").unwrap();
    assert_eq!(fc["call_id"], "call_1");
    let out = input.iter().find(|i| i["type"] == "function_call_output").unwrap();
    let parsed: Value = serde_json::from_str(out["output"].as_str().unwrap()).unwrap();
    assert_eq!(parsed["results"].as_array().unwrap().len(), 2);
    assert_eq!(parsed["results"][0], json!({"source": "handbook.pdf", "content": "VPN s"}));
    // accounting
    let n = env.count("SELECT file_search_completed_count FROM chat_turns").await;
    assert_eq!(n, 1);
    env.eventually("usage", |e| e.usage_events().len() == 1).await;
    let u = &env.usage_events()[0];
    assert_eq!(u.file_search_calls, 1);
    assert_eq!(u.usage.unwrap().input_tokens, 7);
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert_eq!(msgs["items"][1]["content"], "From the handbook.");
}

#[tokio::test]
async fn retrievals_beyond_the_limit_get_search_limit_reached() {
    let env = TestEnv::with(ks_env(1)).await;
    install_search(&env);
    env.proxy.respond(|r| {
        r.uri.contains("/responses").then(|| match outputs_in(r) {
            0 => sse_resp(&call("call_1", "a")),
            1 => sse_resp(&call("call_2", "b")),
            _ => sse_resp(&answer()),
        })
    });
    let chat = env.chat(None).await;
    let r = env.send_msg(&chat, "q").await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    assert_eq!(env.proxy.requests_to("/vector_stores/vs_kb/search").len(), 1, "only max_calls retrievals run");
    let last = env.proxy.chat_requests().last().unwrap().json();
    let outs: Vec<String> = last["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .map(|i| i["output"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(outs.len(), 2);
    assert_eq!(outs[1], "search limit reached");
    env.eventually("usage", |e| e.usage_events().len() == 1).await;
    assert_eq!(env.usage_events()[0].file_search_calls, 2, "in-memory call count");
    assert_eq!(env.count("SELECT file_search_completed_count FROM chat_turns").await, 1);
}

#[tokio::test]
async fn agentic_loop_is_hard_capped() {
    let env = TestEnv::with(ks_env(1)).await;
    install_search(&env);
    env.proxy.respond(|r| r.uri.contains("/responses").then(|| sse_resp(&call("call_n", "again"))));
    let chat = env.chat(None).await;
    let r = env.send_msg(&chat, "q").await;
    assert_eq!(r.event_names().last().unwrap(), "error", "{}", r.text());
    assert_eq!(r.event("error").unwrap()["code"], "agentic_iterations_exceeded");
    assert_eq!(env.proxy.chat_requests().len(), 3, "max_calls + 2 iterations");
    let rows = env.sql_rows("SELECT state, error_code FROM chat_turns").await;
    assert_eq!(rows[0].try_get_by_index::<String>(0).unwrap(), "failed");
    assert_eq!(
        rows[0].try_get_by_index::<Option<String>>(1).unwrap().as_deref(),
        Some("agentic_iterations_exceeded")
    );
    env.eventually("usage", |e| e.usage_events().len() == 1).await;
    let u = &env.usage_events()[0];
    assert_eq!(u.billing_outcome, "failed");
    assert_eq!(u.settlement_method, "actual", "the last iteration reported usage");
}

#[tokio::test]
async fn other_function_tools_are_unexpected_even_with_knowledge_search() {
    let env = TestEnv::with(ks_env(3)).await;
    env.proxy.respond(|r| {
        r.uri.contains("/responses").then(|| {
            sse_resp(&[(
                "response.output_item.done".into(),
                json!({"item": {"type": "function_call", "call_id": "c", "name": "load_files", "arguments": "{}"}}),
            )])
        })
    });
    let chat = env.chat(None).await;
    let r = env.send_msg(&chat, "q").await;
    assert_eq!(r.event("error").unwrap()["code"], "unexpected_tool_use");
}

#[tokio::test]
async fn file_search_wins_over_knowledge_search() {
    let env = TestEnv::with(ks_env(3)).await;
    let chat = env.chat(None).await;
    // a ready document in the chat
    let boundary = "B0UND";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.4 x\r\n--{boundary}--\r\n"
    );
    let mut req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
        .header("content-type", format!("multipart/form-data; boundary={boundary}"))
        .body(axum::body::Body::from(body))
        .unwrap();
    req.extensions_mut().insert(user_a());
    assert_eq!(env.send(req).await.status, 201);
    let r = env.send_msg(&chat, "q").await;
    assert_eq!(r.event_names().last().unwrap(), "done");
    let req = env.proxy.chat_requests()[0].json();
    let tools = req["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["type"] == "file_search"));
    assert!(!tools.iter().any(|t| t["name"] == "search_knowledge"), "never both tools");
    let guard = &env.svc.cfg.knowledge_search.guard;
    assert!(!req["instructions"].as_str().unwrap().contains(guard.as_str()));
}

#[tokio::test]
async fn knowledge_search_off_when_provider_cannot_serve_it() {
    // no api_version on the provider entry → feature off for the request
    let env = TestEnv::with(EnvOptions {
        config: json!({"knowledge_search": {"enabled": true, "vector_store_id": "vs_kb", "provider_id": "mock"}}),
        ..Default::default()
    })
    .await;
    let chat = env.chat(None).await;
    env.send_msg(&chat, "q").await;
    let req = env.proxy.chat_requests()[0].json();
    assert!(req.get("tools").is_none());
}
