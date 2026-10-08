#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};

use super::OpenAiChatCompletionsAdapter;
use crate::infra::llm::adapter_fixtures::{TENANT, USER, feed, knowledge_round, request};
use crate::infra::llm::{FunctionCall, LlmEvent, LlmTool, LlmUsage, ProviderAdapter};

fn all_tools() -> Vec<LlmTool> {
    vec![
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
    ]
}

fn chunk(v: Value) -> (&'static str, Value) {
    ("", v)
}

#[test]
fn body_uses_messages_and_stream_options() {
    let mut req = request(all_tools());
    req.api_params.temperature = Some(0.5);
    req.input[2].image_file_ids = vec!["file-img".to_owned()];
    let b = OpenAiChatCompletionsAdapter.build_body(&req);

    assert_eq!(b["model"], json!("model-x"));
    assert_eq!(
        b["messages"],
        json!([
            {"role": "system", "content": "Be helpful."},
            {"role": "user", "content": "earlier question"},
            {"role": "assistant", "content": "earlier answer"},
            // Chat Completions has no file ids: the image part is dropped.
            {"role": "user", "content": "hi"},
        ])
    );
    assert_eq!(b["stream"], json!(true));
    assert_eq!(b["stream_options"], json!({"include_usage": true}));
    assert_eq!(b["max_completion_tokens"], json!(4096));
    assert!(b.get("max_tokens").is_none());
    assert!(b.get("max_output_tokens").is_none());
    assert_eq!(
        b["user"],
        json!(format!("{}{}", TENANT.simple(), USER.simple()))
    );
    assert!(b.get("metadata").is_none());
    assert!(b.get("max_tool_calls").is_none());
    assert!(b.get("include").is_none());
    assert!(b.get("input").is_none());
    assert!(b.get("instructions").is_none());
    assert_eq!(b["temperature"], json!(0.5));
    // Built-in tools dropped, the function tool kept.
    let tools = b["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], json!("function"));
    assert_eq!(tools[0]["function"]["name"], json!("search_knowledge"));
    assert_eq!(
        tools[0]["function"]["parameters"]["required"],
        json!(["query"])
    );

    // Only built-in tools: no `tools` key at all.
    let b = OpenAiChatCompletionsAdapter.build_body(&request(vec![LlmTool::WebSearch {
        context_size: "low".to_owned(),
    }]));
    assert!(b.get("tools").is_none());

    // Non-streaming (summary): no stream_options.
    let mut req = request(Vec::new());
    req.stream = false;
    let b = OpenAiChatCompletionsAdapter.build_body(&req);
    assert_eq!(b["stream"], json!(false));
    assert!(b.get("stream_options").is_none());
}

#[test]
fn extra_body_merged_except_controlled_keys() {
    let mut req = request(Vec::new());
    let mut extra = serde_json::Map::new();
    extra.insert("seed".to_owned(), json!(7));
    extra.insert("messages".to_owned(), json!([]));
    req.api_params.extra_body = Some(extra);
    let b = OpenAiChatCompletionsAdapter.build_body(&req);
    assert_eq!(b["seed"], json!(7));
    assert_eq!(b["messages"].as_array().unwrap().len(), 4);
}

#[test]
fn parses_choices_delta_and_usage() {
    let events = feed(
        &OpenAiChatCompletionsAdapter,
        &[
            chunk(
                json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]}),
            ),
            chunk(
                json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "Hel"}, "finish_reason": null}]}),
            ),
            chunk(
                json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": null}]}),
            ),
            chunk(
                json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ),
            chunk(json!({"id": "chatcmpl-1", "choices": [], "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 7,
                "total_tokens": 19,
                "prompt_tokens_details": {"cached_tokens": 3},
                "completion_tokens_details": {"reasoning_tokens": 2},
            }})),
            chunk(Value::String("[DONE]".to_owned())),
        ],
    );
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("Hel".to_owned()),
            LlmEvent::TextDelta("lo".to_owned()),
            LlmEvent::Completed {
                usage: Some(LlmUsage {
                    input_tokens: 12,
                    output_tokens: 7,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: 2,
                }),
                response_id: Some("chatcmpl-1".to_owned()),
                incomplete_reason: None,
            },
        ]
    );

    // `length` is a truncated completion; `[DONE]` without usage still ends it.
    let events = feed(
        &OpenAiChatCompletionsAdapter,
        &[
            chunk(
                json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": "length"}]}),
            ),
            chunk(Value::String("[DONE]".to_owned())),
        ],
    );
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("x".to_owned()),
            LlmEvent::Completed {
                usage: None,
                response_id: Some("chatcmpl-2".to_owned()),
                incomplete_reason: Some("max_tokens".to_owned()),
            },
        ]
    );

    // An error chunk fails the stream.
    let events = feed(
        &OpenAiChatCompletionsAdapter,
        &[chunk(
            json!({"error": {"message": "model overloaded", "type": "server_error"}}),
        )],
    );
    assert!(
        matches!(&events[..], [LlmEvent::Failed { error, usage: None }] if error.to_string().contains("model overloaded")),
        "{events:?}"
    );
}

#[test]
fn completion_body_parsed() {
    let body = json!({
        "id": "chatcmpl-9",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Summary text"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 30, "completion_tokens": 4},
    });
    let r = OpenAiChatCompletionsAdapter
        .parse_completion(body.to_string().as_bytes())
        .unwrap();
    assert_eq!(r.text, "Summary text");
    assert_eq!(r.usage.unwrap().input_tokens, 30);
    assert_eq!(r.usage.unwrap().output_tokens, 4);

    let err = OpenAiChatCompletionsAdapter
        .parse_completion(json!({"error": {"message": "bad"}}).to_string().as_bytes())
        .unwrap_err();
    assert!(err.to_string().contains("bad"));
}

#[test]
fn function_call_tool_events() {
    let tc = |v: Value| {
        chunk(
            json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {"tool_calls": [v]}, "finish_reason": null}]}),
        )
    };
    let events = feed(
        &OpenAiChatCompletionsAdapter,
        &[
            tc(
                json!({"index": 0, "id": "call_1", "type": "function", "function": {"name": "search_knowledge", "arguments": ""}}),
            ),
            tc(json!({"index": 0, "function": {"arguments": "{\"query\":"}})),
            tc(json!({"index": 0, "function": {"arguments": "\"vacation policy\"}"}})),
            chunk(
                json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            ),
            chunk(
                json!({"id": "chatcmpl-3", "choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 9}}),
            ),
            chunk(Value::String("[DONE]".to_owned())),
        ],
    );
    let args = r#"{"query":"vacation policy"}"#;
    assert_eq!(
        events,
        vec![
            LlmEvent::ToolStart {
                name: "function_call".to_owned(),
                details: json!({"index": 0, "call_id": "call_1", "name": "search_knowledge"}),
            },
            LlmEvent::ToolDone {
                name: "function_call".to_owned(),
                details: json!({"call_id": "call_1", "name": "search_knowledge", "arguments": args}),
            },
            LlmEvent::FunctionCall(FunctionCall {
                call_id: "call_1".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: args.to_owned(),
            }),
            LlmEvent::Completed {
                usage: Some(LlmUsage {
                    input_tokens: 5,
                    output_tokens: 9,
                    ..LlmUsage::default()
                }),
                response_id: Some("chatcmpl-3".to_owned()),
                incomplete_reason: None,
            },
        ]
    );

    // The finished round goes back as an assistant tool call and a tool message.
    let mut req = request(vec![LlmTool::SearchKnowledge]);
    req.tool_rounds = vec![knowledge_round("[\"chunk\"]")];
    let b = OpenAiChatCompletionsAdapter.build_body(&req);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 6);
    assert_eq!(
        msgs[4],
        json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_1", "type": "function", "function": {"name": "search_knowledge", "arguments": "{\"query\":\"q\"}"}},
        ]})
    );
    assert_eq!(
        msgs[5],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "[\"chunk\"]"})
    );
}
