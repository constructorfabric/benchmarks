#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};

use super::AnthropicMessagesAdapter;
use crate::infra::llm::adapter_fixtures::{TENANT, USER, feed, knowledge_round, request};
use crate::infra::llm::{FunctionCall, LlmEvent, LlmTool, LlmUsage, ProviderAdapter, RawCitation};

fn ev(name: &'static str, mut data: Value) -> (&'static str, Value) {
    data["type"] = json!(name);
    (name, data)
}

fn header<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn body_has_system_messages_max_tokens_metadata_user_id() {
    let mut req = request(Vec::new());
    req.api_params.temperature = Some(0.2);
    req.api_params.frequency_penalty = Some(1.0);
    req.api_params.stop = vec!["END".to_owned()];
    let mut extra = serde_json::Map::new();
    extra.insert("seed".to_owned(), json!(7));
    req.api_params.extra_body = Some(extra);
    req.input[2].image_file_ids = vec!["file_sec_1".to_owned()];
    let b = AnthropicMessagesAdapter.build_body(&req);

    assert_eq!(b["model"], json!("model-x"));
    assert_eq!(b["system"], json!("Be helpful."));
    assert_eq!(
        b["messages"],
        json!([
            {"role": "user", "content": "earlier question"},
            {"role": "assistant", "content": "earlier answer"},
            {"role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "image", "source": {"type": "file", "file_id": "file_sec_1"}},
            ]},
        ])
    );
    assert_eq!(b["max_tokens"], json!(4096));
    assert_eq!(b["stream"], json!(true));
    assert_eq!(
        b["metadata"],
        json!({"user_id": format!("{}{}", TENANT.simple(), USER.simple())})
    );
    assert!(b.get("user").is_none());
    assert!(b.get("instructions").is_none());
    assert!(b.get("input").is_none());
    assert!(b.get("max_output_tokens").is_none());
    assert!(b.get("tools").is_none());
    // extra_body is not sent; unsupported penalties are not sent.
    assert!(b.get("seed").is_none());
    assert!(b.get("frequency_penalty").is_none());
    assert_eq!(b["temperature"], json!(0.2));
    assert_eq!(b["stop_sequences"], json!(["END"]));

    let headers = AnthropicMessagesAdapter.headers(&req);
    assert_eq!(header(&headers, "anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        header(&headers, "anthropic-beta"),
        Some("files-api-2025-04-14")
    );
    let plain = AnthropicMessagesAdapter.headers(&request(Vec::new()));
    assert_eq!(header(&plain, "anthropic-version"), Some("2023-06-01"));
    assert_eq!(header(&plain, "anthropic-beta"), None);
}

#[test]
fn drops_file_search_maps_web_search_and_code_execution() {
    let req = request(vec![
        LlmTool::FileSearch {
            vector_store_id: "vs_1".to_owned(),
            max_num_results: 5,
        },
        LlmTool::WebSearch {
            context_size: "low".to_owned(),
        },
        LlmTool::CodeInterpreter {
            file_ids: vec!["file-x".to_owned()],
        },
        LlmTool::SearchKnowledge,
    ]);
    let b = AnthropicMessagesAdapter.build_body(&req);
    let tools = b["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3, "{tools:?}");
    assert_eq!(
        tools[0],
        json!({"type": "web_search_20250305", "name": "web_search"})
    );
    assert_eq!(
        tools[1],
        json!({"type": "code_execution_20250522", "name": "code_execution"})
    );
    assert_eq!(tools[2]["name"], json!("search_knowledge"));
    assert_eq!(tools[2]["input_schema"]["required"], json!(["query"]));
    assert!(tools[2].get("type").is_none());
    assert!(b.get("max_tool_calls").is_none());

    let headers = AnthropicMessagesAdapter.headers(&req);
    assert_eq!(
        header(&headers, "anthropic-beta"),
        Some("code-execution-2025-05-22")
    );

    // A finished knowledge round: assistant tool_use, then the user tool_result.
    let mut req = request(vec![LlmTool::SearchKnowledge]);
    req.tool_rounds = vec![knowledge_round("[\"chunk\"]")];
    let b = AnthropicMessagesAdapter.build_body(&req);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5);
    assert_eq!(
        msgs[3],
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "call_1", "name": "search_knowledge", "input": {"query": "q"}},
        ]})
    );
    assert_eq!(
        msgs[4],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "call_1", "content": "[\"chunk\"]"},
        ]})
    );
}

#[test]
fn parses_content_block_deltas_and_message_delta_usage() {
    let events = feed(
        &AnthropicMessagesAdapter,
        &[
            ev(
                "message_start",
                json!({"message": {"id": "msg_1", "type": "message", "role": "assistant", "content": [],
                    "usage": {"input_tokens": 20, "cache_read_input_tokens": 5, "cache_creation_input_tokens": 2, "output_tokens": 1}}}),
            ),
            ev(
                "content_block_start",
                json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            ev("ping", json!({})),
            ev(
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
            ),
            ev(
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
            ),
            ev(
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "citations_delta", "citation": {
                    "type": "web_search_result_location", "url": "https://example.com/a",
                    "title": "Example", "cited_text": "cited", "encrypted_index": "x"}}}),
            ),
            ev("content_block_stop", json!({"index": 0})),
            ev(
                "content_block_start",
                json!({"index": 1, "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}}),
            ),
            ev("content_block_stop", json!({"index": 1})),
            ev(
                "content_block_start",
                json!({"index": 2, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []}}),
            ),
            ev("content_block_stop", json!({"index": 2})),
            ev(
                "content_block_start",
                json!({"index": 3, "content_block": {"type": "server_tool_use", "id": "srvtoolu_2", "name": "code_execution", "input": {}}}),
            ),
            ev("content_block_stop", json!({"index": 3})),
            ev(
                "content_block_start",
                json!({"index": 4, "content_block": {"type": "thinking", "thinking": ""}}),
            ),
            ev(
                "content_block_delta",
                json!({"index": 4, "delta": {"type": "thinking_delta", "thinking": "secret"}}),
            ),
            ev("content_block_stop", json!({"index": 4})),
            ev(
                "content_block_start",
                json!({"index": 5, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {}}}),
            ),
            ev(
                "content_block_delta",
                json!({"index": 5, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": "}}),
            ),
            ev(
                "content_block_delta",
                json!({"index": 5, "delta": {"type": "input_json_delta", "partial_json": "\"q\"}"}}),
            ),
            ev("content_block_stop", json!({"index": 5})),
            ev(
                "message_delta",
                json!({"delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 15}}),
            ),
            ev("message_stop", json!({})),
        ],
    );
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("Hel".to_owned()),
            LlmEvent::TextDelta("lo".to_owned()),
            LlmEvent::Citation(RawCitation::Web {
                url: "https://example.com/a".to_owned(),
                title: "Example".to_owned(),
                snippet: "cited".to_owned(),
                span: None,
            }),
            LlmEvent::ToolStart {
                name: "web_search".to_owned(),
                details: json!({}),
            },
            LlmEvent::ToolDone {
                name: "web_search".to_owned(),
                details: json!({}),
            },
            LlmEvent::ToolStart {
                name: "code_interpreter".to_owned(),
                details: json!({}),
            },
            LlmEvent::ToolDone {
                name: "code_interpreter".to_owned(),
                details: json!({}),
            },
            // Thinking is not emitted (only the vLLM adapter emits reasoning).
            LlmEvent::ToolStart {
                name: "search_knowledge".to_owned(),
                details: json!({}),
            },
            LlmEvent::FunctionCall(FunctionCall {
                call_id: "toolu_1".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: r#"{"query": "q"}"#.to_owned(),
            }),
            LlmEvent::Completed {
                // Anthropic reports cache tokens separately; normalized they are
                // a subset of input_tokens (DESIGN §5.5.9): 20 + 5 + 2.
                usage: Some(LlmUsage {
                    input_tokens: 27,
                    output_tokens: 15,
                    cache_read_input_tokens: 5,
                    cache_write_input_tokens: 2,
                    reasoning_tokens: 0,
                }),
                response_id: Some("msg_1".to_owned()),
                incomplete_reason: None,
            },
        ]
    );

    // Function tool starts are named by the tool: load_files, else unknown_tool.
    let starts = feed(
        &AnthropicMessagesAdapter,
        &[
            ev(
                "content_block_start",
                json!({"index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "load_files", "input": {}}}),
            ),
            ev(
                "content_block_start",
                json!({"index": 1, "content_block": {"type": "tool_use", "id": "t2", "name": "do_magic", "input": {}}}),
            ),
        ],
    );
    let names: Vec<_> = starts
        .iter()
        .map(|e| match e {
            LlmEvent::ToolStart { name, .. } => name.as_str(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(names, ["load_files", "unknown_tool"]);

    // An `error` event fails the stream.
    let events = feed(
        &AnthropicMessagesAdapter,
        &[ev(
            "error",
            json!({"error": {"type": "overloaded_error", "message": "Overloaded"}}),
        )],
    );
    assert!(
        matches!(&events[..], [LlmEvent::Failed { error, .. }] if error.to_string().contains("Overloaded")),
        "{events:?}"
    );
}

#[test]
fn max_tokens_stop_is_incomplete() {
    let events = feed(
        &AnthropicMessagesAdapter,
        &[
            ev(
                "message_start",
                json!({"message": {"id": "msg_2", "usage": {"input_tokens": 3, "output_tokens": 1}}}),
            ),
            ev(
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "text_delta", "text": "cut"}}),
            ),
            ev(
                "message_delta",
                json!({"delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 4096}}),
            ),
            ev("message_stop", json!({})),
        ],
    );
    assert_eq!(
        events.last().unwrap(),
        &LlmEvent::Completed {
            usage: Some(LlmUsage {
                input_tokens: 3,
                output_tokens: 4096,
                ..LlmUsage::default()
            }),
            response_id: Some("msg_2".to_owned()),
            incomplete_reason: Some("max_tokens".to_owned()),
        }
    );
}

#[test]
fn completion_body_parsed() {
    let body = json!({
        "id": "msg_9", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "Sum"}, {"type": "text", "text": "mary"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 30, "output_tokens": 4},
    });
    let r = AnthropicMessagesAdapter
        .parse_completion(body.to_string().as_bytes())
        .unwrap();
    assert_eq!(r.text, "Summary");
    assert_eq!(r.usage.unwrap().input_tokens, 30);

    let err = AnthropicMessagesAdapter
        .parse_completion(
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "prompt is too long"}})
                .to_string()
                .as_bytes(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("prompt is too long"));
}
