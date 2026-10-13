#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::too_many_lines)]
//! Slice 7, end to end: the knowledge-search loop through the real OAGW
//! retriever and the fake gateway, and the other adapters
//! (`openai_chat_completions`, `vllm_responses`, `anthropic_messages`) from the
//! provider request to the SSE contract: equivalent provider streams give the
//! same SSE events.

use serde_json::{Value, json};

use crate::config::MiniChatConfig;
use crate::domain::service::quota::test_support::{design_catalog, model};
use crate::domain::service::stream::test_support::StreamMetric;
use crate::stream_test_env::{Options, StreamEnv, oa, rid};
use crate::test_doubles::{ProxyReply, provider_json};
use mini_chat_sdk::ModelTier;

const SEARCH_URI: &str =
    "/kb.azure.test/openai/vector_stores/vs_kb/search?api-version=2025-01-01-preview";

fn function_call(call_id: &str, query: &str, input: u64, output: u64) -> (&'static str, Value) {
    (
        "response.completed",
        json!({ "type": "response.completed", "response": {
            "id": "resp_tool",
            "usage": { "input_tokens": input, "output_tokens": output },
            "output": [ { "type": "function_call", "call_id": call_id, "name": "search_knowledge",
                          "arguments": json!({ "query": query }).to_string() } ]
        } }),
    )
}

fn search_hit(text: &str) -> ProxyReply {
    ProxyReply::json(
        200,
        &json!({ "data": [ { "file_id": "file-kb", "filename": "kb.md", "score": 0.9,
                             "content": [ { "type": "text", "text": text } ] } ] }),
    )
}

async fn knowledge_env() -> StreamEnv {
    StreamEnv::with(Options {
        knowledge: true,
        ..Options::default()
    })
    .await
}

fn kinds() -> Vec<(&'static str, Value)> {
    let mut chat_completions = provider_json("cc.test", "openai");
    chat_completions["kind"] = json!("openai_chat_completions");
    chat_completions["api_path"] = json!("/v1/chat/completions");
    let mut vllm = provider_json("vllm.test", "openai");
    vllm["kind"] = json!("vllm_responses");
    vllm["api_path"] = json!("/v1/responses");
    let mut anthropic = provider_json("anthropic.test", "openai");
    anthropic["kind"] = json!("anthropic_messages");
    anthropic["api_path"] = json!("/v1/messages");
    vec![
        ("p_cc", chat_completions),
        ("p_vllm", vllm),
        ("p_an", anthropic),
    ]
}

fn catalog() -> Vec<mini_chat_sdk::ModelCatalogEntry> {
    let mut catalog = design_catalog();
    for (id, provider) in [("cc", "p_cc"), ("vl", "p_vllm"), ("an", "p_an")] {
        catalog.push(model(id, ModelTier::Standard).provider(provider).build());
    }
    catalog
}

async fn adapter_env(knowledge: bool) -> StreamEnv {
    StreamEnv::with(Options {
        catalog: catalog(),
        providers: kinds(),
        knowledge,
        ..Options::default()
    })
    .await
}

fn chunk(delta: &Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({ "id": "chatcmpl-9", "choices": [ { "index": 0, "delta": delta, "finish_reason": finish } ], "usage": null })
    )
}

fn cc_usage(prompt: u64, completion: u64) -> String {
    format!(
        "data: {}\n\n",
        json!({ "id": "chatcmpl-9", "choices": [], "usage": { "prompt_tokens": prompt, "completion_tokens": completion } })
    )
}

fn anthropic_events(texts: &[&str], input: u64, output: u64) -> Vec<(&'static str, Value)> {
    let mut events = vec![(
        "message_start",
        json!({ "type": "message_start", "message": { "id": "msg_9",
                "usage": { "input_tokens": input, "output_tokens": 1 } } }),
    )];
    for text in texts {
        events.push((
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": text } }),
        ));
    }
    events.push((
        "message_delta",
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": output } }),
    ));
    events.push(("message_stop", json!({ "type": "message_stop" })));
    events
}

// ==== the knowledge-search loop ====================================================================

#[tokio::test]
async fn a_search_call_runs_the_retrieval_and_the_answer_streams_from_the_next_request() {
    let env = knowledge_env().await;
    env.script(&[
        oa::delta("Let me check. "),
        function_call("call_1", "refund policy", 10, 1),
    ]);
    env.oagw
        .push_proxy(search_hit("Refunds are paid within 5 days."));
    env.script(&[oa::delta("Five days."), oa::completed(50, 9)]);
    let request = rid(1);
    let streamed = env
        .stream(
            env.chat,
            &json!({ "content": "how long do refunds take?", "request_id": request }),
        )
        .await;

    // The same SSE contract as without knowledge search: no tool event for the
    // function call of the Responses adapter.
    assert_eq!(
        streamed.names(),
        ["stream_started", "delta", "delta", "done"]
    );
    assert_eq!(streamed.text(), "Let me check. Five days.");
    assert_eq!(
        streamed.last().data["usage"],
        json!({ "input_tokens": 50, "output_tokens": 9 }),
        "only the last iteration is settled"
    );

    let sent = env.oagw.proxied();
    assert_eq!(sent.len(), 3, "provider, retrieval, provider");
    assert_eq!(sent[1].method, "POST");
    assert_eq!(sent[1].uri, SEARCH_URI);
    assert_eq!(
        sent[1].json(),
        json!({ "query": "refund policy", "max_num_results": 5 })
    );
    assert_eq!(sent[1].subject, sent[0].subject);

    // The first request offers the tool and the guard; the second one carries
    // the call and its output and the same tools.
    let first = sent[0].json();
    assert_eq!(first["tools"][0]["type"], "function");
    assert_eq!(first["tools"][0]["name"], "search_knowledge");
    assert!(
        first["instructions"]
            .as_str()
            .unwrap()
            .ends_with(&MiniChatConfig::default().knowledge_search.guard)
    );
    let second = sent[2].json();
    assert_eq!(second["tools"], first["tools"]);
    assert_eq!(second["store"], false);
    let input = second["input"].as_array().unwrap();
    assert_eq!(input.len(), first["input"].as_array().unwrap().len() + 2);
    assert_eq!(
        input[input.len() - 2],
        json!({ "type": "function_call", "call_id": "call_1", "name": "search_knowledge",
                "arguments": "{\"query\":\"refund policy\"}" })
    );
    assert_eq!(
        input[input.len() - 1],
        json!({ "type": "function_call_output", "call_id": "call_1",
                "output": "{\"results\":[{\"text\":\"Refunds are paid within 5 days.\"}]}" })
    );

    // Counted once in the turn, the usage event and the audit event.
    assert_eq!(
        env.turn_text(request, "file_search_completed_count")
            .await
            .as_deref(),
        Some("1")
    );
    assert_eq!(env.usage_events().await[0].file_search_calls, 1);
    assert_eq!(env.audit_events().await[0].tool_calls.file_search, 1);
    assert_eq!(
        env.metrics
            .count(|m| matches!(m, StreamMetric::KnowledgeSearch(r) if r == "ok")),
        1
    );
    let messages = env.messages(env.chat).await;
    assert_eq!(messages.last().unwrap().1, "Let me check. Five days.");
}

#[tokio::test]
async fn a_failed_retrieval_counts_as_a_call_but_not_as_a_completed_search() {
    let env = knowledge_env().await;
    env.script(&[function_call("call_1", "first", 10, 1)]);
    env.oagw.push_proxy(ProxyReply::json(
        502,
        &json!({ "error": { "message": "bad gateway" } }),
    ));
    env.script(&[function_call("call_2", "second", 20, 2)]);
    env.oagw.push_proxy(search_hit("found it"));
    env.script(&[oa::delta("done"), oa::completed(30, 3)]);
    let request = rid(2);
    let streamed = env
        .stream(env.chat, &json!({ "content": "q", "request_id": request }))
        .await;
    assert_eq!(streamed.names(), ["stream_started", "delta", "done"]);

    let sent = env.oagw.proxied();
    assert_eq!(sent.len(), 5);
    let third = sent[4].json();
    let input = third["input"].as_array().unwrap();
    assert_eq!(
        input[input.len() - 4]["call_id"],
        "call_1",
        "the failed call is answered, not dropped"
    );
    assert_eq!(
        input[input.len() - 3],
        json!({ "type": "function_call_output", "call_id": "call_1",
                "output": "{\"error\":\"search failed\"}" })
    );
    assert_eq!(
        input[input.len() - 1]["output"],
        "{\"results\":[{\"text\":\"found it\"}]}"
    );

    assert_eq!(
        env.turn_text(request, "file_search_completed_count")
            .await
            .as_deref(),
        Some("1")
    );
    assert_eq!(env.usage_events().await[0].file_search_calls, 2);
    assert_eq!(env.audit_events().await[0].tool_calls.file_search, 2);
    let knowledge: Vec<_> = env
        .metrics
        .events()
        .into_iter()
        .filter_map(|metric| match metric {
            StreamMetric::KnowledgeSearch(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(knowledge, ["error", "ok"]);
}

#[tokio::test]
async fn endless_search_calls_end_with_agentic_iterations_exceeded() {
    let env = knowledge_env().await;
    // max_calls_per_message is 3: five provider iterations are allowed.
    for n in 0..5 {
        env.script(&[function_call(&format!("call_{n}"), "again", 10 + n, 1)]);
        if n < 3 {
            env.oagw.push_proxy(search_hit("same"));
        }
    }
    let request = rid(3);
    let streamed = env
        .stream(env.chat, &json!({ "content": "q", "request_id": request }))
        .await;
    assert_eq!(streamed.names(), ["stream_started", "error"]);
    assert_eq!(streamed.last().data["code"], "agentic_iterations_exceeded");
    assert_eq!(
        env.turn_text(request, "error_code").await.as_deref(),
        Some("agentic_iterations_exceeded")
    );
    assert_eq!(
        env.oagw.proxy_call_count(),
        5 + 3,
        "five provider requests and three retrievals; the rest got the limit output"
    );
    assert_eq!(env.usage_events().await[0].file_search_calls, 4);
}

#[tokio::test]
async fn another_function_is_unexpected_tool_use_even_with_knowledge_search() {
    let env = knowledge_env().await;
    env.script(&[(
        "response.completed",
        json!({ "response": { "id": "r", "usage": { "input_tokens": 9, "output_tokens": 1 },
            "output": [ { "type": "function_call", "call_id": "c1", "name": "load_files", "arguments": "{}" } ] } }),
    )]);
    let request = rid(4);
    let streamed = env
        .stream(env.chat, &json!({ "content": "q", "request_id": request }))
        .await;
    assert_eq!(streamed.last().data["code"], "unexpected_tool_use");
    assert_eq!(
        env.oagw.proxy_call_count(),
        1,
        "no retrieval, no second request"
    );
}

#[tokio::test]
async fn without_the_configuration_the_request_has_no_search_knowledge_tool() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(env.chat, &json!({ "content": "q" })).await;
    let sent = env.oagw.proxied();
    assert!(sent[0].json().get("tools").is_none());
    assert!(
        !sent[0].json()["instructions"]
            .as_str()
            .unwrap()
            .contains(&MiniChatConfig::default().knowledge_search.guard)
    );
}

// ==== the loop on the other adapters ===============================================================

#[tokio::test]
async fn the_chat_completions_loop_answers_the_tool_call_with_a_tool_message() {
    let env = adapter_env(true).await;
    let chat = env.seed_chat(2, "cc").await;
    env.oagw.push_proxy(ProxyReply::sse_text(
        [
            chunk(&json!({ "tool_calls": [ { "index": 0, "id": "call_a", "type": "function",
                    "function": { "name": "search_knowledge", "arguments": "{\"query\":\"refunds\"}" } } ] }), None),
            chunk(&json!({}), Some("tool_calls")),
            cc_usage(10, 2),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat(),
    ));
    env.oagw.push_proxy(search_hit("five days"));
    env.oagw.push_proxy(ProxyReply::sse_text(
        [
            chunk(&json!({ "content": "Five days." }), None),
            chunk(&json!({}), Some("stop")),
            cc_usage(40, 6),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat(),
    ));
    let request = rid(5);
    let streamed = env
        .stream(
            chat,
            &json!({ "content": "refunds?", "request_id": request }),
        )
        .await;
    assert_eq!(
        streamed.names(),
        ["stream_started", "tool", "tool", "delta", "done"],
        "the Chat Completions adapter reports its function call"
    );
    assert_eq!(
        streamed.frames[1].data,
        json!({ "phase": "start", "name": "function_call",
                "details": { "index": 0, "call_id": "call_a", "name": "search_knowledge" } })
    );
    assert_eq!(
        streamed.frames[2].data,
        json!({ "phase": "done", "name": "function_call",
                "details": { "call_id": "call_a", "name": "search_knowledge",
                             "arguments": "{\"query\":\"refunds\"}" } })
    );
    assert_eq!(
        streamed.last().data["usage"],
        json!({ "input_tokens": 40, "output_tokens": 6 })
    );
    let sent = env.oagw.proxied();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0].uri, "/cc.test/v1/chat/completions");
    assert_eq!(
        sent[0].json()["tools"][0]["function"]["name"],
        "search_knowledge"
    );
    let messages = sent[2].json()["messages"].as_array().unwrap().clone();
    let tail = &messages[messages.len() - 2..];
    assert_eq!(tail[0]["role"], "assistant");
    assert_eq!(tail[0]["tool_calls"][0]["id"], "call_a");
    assert_eq!(
        tail[1],
        json!({ "role": "tool", "tool_call_id": "call_a",
                "content": "{\"results\":[{\"text\":\"five days\"}]}" })
    );
    assert_eq!(env.usage_events().await[0].file_search_calls, 1);
}

#[tokio::test]
async fn the_anthropic_loop_answers_the_tool_use_with_a_tool_result() {
    let env = adapter_env(true).await;
    let chat = env.seed_chat(3, "an").await;
    env.script(&[
        (
            "message_start",
            json!({ "type": "message_start", "message": { "id": "msg_1",
                    "usage": { "input_tokens": 10, "output_tokens": 1 } } }),
        ),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {} } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"query\":\"refunds\"}" } }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]);
    env.oagw.push_proxy(search_hit("five days"));
    env.script(&anthropic_events(&["Five days."], 40, 6));
    let request = rid(6);
    let streamed = env
        .stream(
            chat,
            &json!({ "content": "refunds?", "request_id": request }),
        )
        .await;
    assert_eq!(
        streamed.names(),
        ["stream_started", "tool", "delta", "done"],
        "a start event named search_knowledge and no done event"
    );
    assert_eq!(
        streamed.frames[1].data,
        json!({ "phase": "start", "name": "search_knowledge", "details": {} })
    );
    assert_eq!(
        streamed.last().data["usage"],
        json!({ "input_tokens": 40, "output_tokens": 6 })
    );
    let sent = env.oagw.proxied();
    assert_eq!(sent[0].uri, "/anthropic.test/v1/messages");
    assert_eq!(sent[0].json()["tools"][0]["name"], "search_knowledge");
    let messages = sent[2].json()["messages"].as_array().unwrap().clone();
    let tail = &messages[messages.len() - 2..];
    assert_eq!(
        tail[0],
        json!({ "role": "assistant", "content": [
            { "type": "tool_use", "id": "toolu_1", "name": "search_knowledge",
              "input": { "query": "refunds" } } ] })
    );
    assert_eq!(
        tail[1],
        json!({ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_1",
              "content": "{\"results\":[{\"text\":\"five days\"}]}" } ] })
    );
    assert_eq!(env.usage_events().await[0].file_search_calls, 1);
}

// ==== the same SSE contract from every adapter =====================================================

#[tokio::test]
async fn equivalent_provider_streams_give_the_same_sse_events() {
    let env = adapter_env(false).await;
    let chats = [
        ("p", env.chat),
        ("cc", env.seed_chat(2, "cc").await),
        ("vl", env.seed_chat(3, "vl").await),
        ("an", env.seed_chat(4, "an").await),
    ];
    for (n, (model, chat)) in chats.into_iter().enumerate() {
        match model {
            "p" => env.script(&[oa::delta("Hel"), oa::delta("lo"), oa::completed(11, 7)]),
            "vl" => env.script(&[
                oa::delta("Hel"),
                oa::delta("lo"),
                (
                    "response.completed",
                    json!({ "response": { "id": "r", "usage": { "input_tokens": 11, "output_tokens": 7 } } }),
                ),
            ]),
            "cc" => env.oagw.push_proxy(ProxyReply::sse_text(
                [
                    chunk(&json!({ "role": "assistant", "content": "" }), None),
                    chunk(&json!({ "content": "Hel" }), None),
                    chunk(&json!({ "content": "lo" }), None),
                    chunk(&json!({}), Some("stop")),
                    cc_usage(11, 7),
                    "data: [DONE]\n\n".to_owned(),
                ]
                .concat(),
            )),
            _ => env.script(&anthropic_events(&["Hel", "lo"], 11, 7)),
        }
        let streamed = env
            .stream(
                chat,
                &json!({ "content": "hi", "request_id": rid(10 + n as u128) }),
            )
            .await;
        assert_eq!(
            streamed.names(),
            ["stream_started", "delta", "delta", "done"],
            "{model}"
        );
        assert_eq!(streamed.text(), "Hello", "{model}");
        let usage = &streamed.last().data["usage"];
        assert_eq!(
            usage,
            &json!({ "input_tokens": 11, "output_tokens": 7 }),
            "{model}"
        );
        assert!(
            !streamed.reply.text.contains("resp_")
                && !streamed.reply.text.contains("chatcmpl-")
                && !streamed.reply.text.contains("msg_9"),
            "no provider id reaches the client ({model})"
        );
    }
    let uris: Vec<String> = env
        .oagw
        .proxied()
        .iter()
        .map(|call| call.uri.clone())
        .collect();
    assert_eq!(
        uris,
        [
            "/api.openai.test/v1/responses",
            "/cc.test/v1/chat/completions",
            "/vllm.test/v1/responses",
            "/anthropic.test/v1/messages",
        ]
    );
}

#[tokio::test]
async fn the_chat_completions_request_is_the_golden_body_of_the_adapter() {
    let env = adapter_env(false).await;
    let chat = env.seed_chat(2, "cc").await;
    env.oagw.push_proxy(ProxyReply::sse_text(
        [
            chunk(&json!({}), Some("stop")),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat(),
    ));
    env.stream(chat, &json!({ "content": "hi" })).await;
    let sent = env.oagw.proxied();
    let body = sent[0].json();
    assert_eq!(body["model"], "provider-cc");
    assert_eq!(
        body["messages"],
        json!([
            { "role": "system", "content": "SECRET-SYSTEM-PROMPT" },
            { "role": "user", "content": "hi" }
        ])
    );
    assert_eq!(body["max_completion_tokens"], 500);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
    for key in [
        "store",
        "metadata",
        "input",
        "instructions",
        "max_output_tokens",
        "tools",
    ] {
        assert!(body.get(key).is_none(), "`{key}`");
    }
    assert_eq!(sent[0].header("accept"), Some("text/event-stream"));
}

#[tokio::test]
async fn the_anthropic_request_carries_the_version_the_user_id_the_server_tool_and_the_image() {
    let env = adapter_env(false).await;
    let chat = env.seed_chat(3, "an").await;
    let image = env
        .seed_attachment(chat, 7, |row| {
            row.set("attachment_kind", "image")
                .set("filename", "pic.png")
                .set("for_file_search", false)
                .set("secondary_file_id", "sec-img-7")
        })
        .await;
    env.script(&[
        (
            "message_start",
            json!({ "type": "message_start", "message": { "id": "msg_1",
                    "usage": { "input_tokens": 20, "cache_read_input_tokens": 5, "output_tokens": 1 } } }),
        ),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {} } }),
        ),
        (
            "content_block_start",
            json!({ "type": "content_block_start", "index": 1,
                    "content_block": { "type": "web_search_tool_result", "tool_use_id": "srv_1", "content": [] } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 2,
                    "delta": { "type": "text_delta", "text": "Rust 1.80 is out." } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 2,
                    "delta": { "type": "citations_delta", "citation": {
                        "type": "web_search_result_location", "url": "https://example.org/rust",
                        "title": "Rust blog", "cited_text": "Rust 1.80 is out", "encrypted_index": "x" } } }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 8 } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]);
    let request = rid(7);
    let streamed = env
        .stream(
            chat,
            &json!({ "content": "look", "request_id": request, "web_search": { "enabled": true },
                     "attachment_ids": [image] }),
        )
        .await;
    assert_eq!(
        streamed.names(),
        [
            "stream_started",
            "tool",
            "tool",
            "delta",
            "citations",
            "done"
        ]
    );
    assert_eq!(
        streamed.frames[1].data,
        json!({ "phase": "start", "name": "web_search", "details": {} })
    );
    assert_eq!(
        streamed.frames[2].data,
        json!({ "phase": "done", "name": "web_search", "details": {} })
    );
    assert_eq!(
        streamed.of("citations")[0].data,
        json!({ "items": [ { "source": "web", "title": "Rust blog", "snippet": "Rust 1.80 is out",
                             "url": "https://example.org/rust" } ] })
    );
    assert_eq!(
        streamed.last().data["usage"],
        json!({ "input_tokens": 25, "output_tokens": 8 }),
        "the cache read tokens are part of the input tokens"
    );
    let sent = env.oagw.proxied();
    assert_eq!(sent[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        sent[0].header("anthropic-beta"),
        Some("files-api-2025-04-14")
    );
    assert!(sent[0].header("x-api-key").is_none() && sent[0].header("authorization").is_none());
    let body = sent[0].json();
    assert!(
        body["system"]
            .as_str()
            .unwrap()
            .starts_with("SECRET-SYSTEM-PROMPT"),
        "the prompt and the guard of the web search"
    );
    assert_eq!(body["max_tokens"], 500);
    assert_eq!(body["metadata"]["user_id"].as_str().unwrap().len(), 64);
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            { "type": "text", "text": "look" },
            { "type": "image", "source": { "type": "file", "file_id": "sec-img-7" } }
        ])
    );
    assert_eq!(
        body["tools"],
        json!([ { "type": "web_search_20250305", "name": "web_search" } ])
    );
    assert_eq!(env.usage_events().await[0].web_search_calls, 1);
}

#[tokio::test]
async fn vllm_think_text_is_relayed_as_reasoning_and_not_persisted() {
    let env = adapter_env(false).await;
    let chat = env.seed_chat(4, "vl").await;
    env.script(&[
        oa::delta("<think>weigh "),
        oa::delta("both</think>The answer"),
        oa::delta(" is 42."),
        oa::completed(12, 9),
    ]);
    let request = rid(8);
    let streamed = env
        .stream(chat, &json!({ "content": "q", "request_id": request }))
        .await;
    let deltas: Vec<(String, String)> = streamed
        .of("delta")
        .iter()
        .map(|f| {
            (
                f.data["type"].as_str().unwrap().to_owned(),
                f.data["content"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        deltas,
        [
            ("reasoning".to_owned(), "weigh ".to_owned()),
            ("reasoning".to_owned(), "both".to_owned()),
            ("text".to_owned(), "The answer".to_owned()),
            ("text".to_owned(), " is 42.".to_owned()),
        ]
    );
    assert_eq!(streamed.text(), "The answer is 42.");
    let messages = env.messages(chat).await;
    assert_eq!(
        messages.last().unwrap().1,
        "The answer is 42.",
        "the reasoning is not part of the persisted content"
    );
    let body = env.oagw.proxied()[0].json();
    assert_eq!(body["store"], false);
    for key in ["tools", "include", "max_tool_calls", "metadata"] {
        assert!(body.get(key).is_none(), "`{key}`");
    }
    assert_eq!(env.oagw.proxied()[0].uri, "/vllm.test/v1/responses");
}

#[tokio::test]
async fn the_adapter_is_the_one_of_the_provider_of_the_effective_model_after_a_downgrade() {
    let env = StreamEnv::with(Options {
        catalog: vec![
            model("p", ModelTier::Premium).enabled(false).build(),
            model("s", ModelTier::Standard).provider("p_an").build(),
        ],
        providers: kinds(),
        ..Options::default()
    })
    .await;
    env.script(&anthropic_events(&["Hello"], 11, 7));
    let streamed = env
        .stream(env.chat, &json!({ "content": "hi", "request_id": rid(20) }))
        .await;
    let done = &streamed.last().data;
    assert_eq!(done["selected_model"], "p");
    assert_eq!(done["effective_model"], "s");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(
        done["usage"],
        json!({ "input_tokens": 11, "output_tokens": 7 })
    );
    let sent = env.oagw.proxied();
    assert_eq!(sent[0].uri, "/anthropic.test/v1/messages");
    assert_eq!(sent[0].json()["model"], "provider-s");
    assert!(sent[0].header("anthropic-version").is_some());
}
