#![allow(clippy::unwrap_used)]

use mini_chat_sdk::ModelApiParams;
use serde_json::{Value, json};

use super::openai_responses::OpenAiResponses;
use super::sse_parser::SseEvent;
use super::{
    Adapter, DeltaKind, InputMessage, LlmRequest, ProviderEvent, RawCitation, RequestMetadata, Role,
    ToolSpec, feature_label, provider_user_field,
};

fn ev(name: &str, data: &Value) -> SseEvent {
    SseEvent {
        event: Some(name.into()),
        data: data.to_string(),
    }
}

#[test]
#[allow(clippy::cognitive_complexity)] // reason: flat list of request-body assertions
fn body_contains_contract_fields() {
    let tools = vec![
        ToolSpec::FileSearch {
            vector_store_id: "vs_1".into(),
            max_num_results: 5,
        },
        ToolSpec::WebSearch {
            context_size: "low".into(),
        },
        ToolSpec::CodeInterpreter {
            file_ids: vec!["file-x".into()],
        },
    ];
    let mut params = ModelApiParams {
        temperature: Some(0.7),
        ..ModelApiParams::default()
    };
    let mut extra = serde_json::Map::new();
    extra.insert("model".into(), json!("evil"));
    extra.insert("seed".into(), json!(7));
    params.extra_body = Some(extra);
    let r = LlmRequest {
        provider_model_id: "gpt-4.1".into(),
        instructions: "sys".into(),
        input: vec![InputMessage {
            role: Role::User,
            text: "q".into(),
            image_file_ids: vec!["file-img".into()],
            secondary_image_file_ids: vec![],
        }],
        max_output_tokens: 100,
        tools: tools.clone(),
        max_tool_calls: 2,
        api_params: params,
        user: provider_user_field(
            "00000000-df51-5b42-9538-d2b56b7ee953",
            "11111111-6a88-4768-9dfc-6bcd5187d9ed",
        ),
        metadata: RequestMetadata {
            tenant_id: "t".into(),
            user_id: "u".into(),
            chat_id: "c".into(),
            request_type: "chat".into(),
            feature: feature_label(&tools),
        },
        stream: true,
        tool_exchanges: Vec::new(),
    };
    let b = OpenAiResponses::default().build_body(&r);
    assert_eq!(b["model"], "gpt-4.1");
    assert_eq!(b["instructions"], "sys");
    assert_eq!(b["stream"], true);
    assert_eq!(b["max_output_tokens"], 100);
    assert_eq!(b["max_tool_calls"], 2);
    assert_eq!(b["temperature"], 0.7);
    assert_eq!(b["seed"], 7);
    assert_eq!(b["user"].as_str().unwrap().len(), 64);
    assert_eq!(b["metadata"]["feature"], "file_search+web_search+code_interpreter");
    assert_eq!(b["metadata"]["request_type"], "chat");
    assert_eq!(b["tools"][0]["type"], "file_search");
    assert_eq!(b["tools"][0]["vector_store_ids"][0], "vs_1");
    assert_eq!(b["tools"][0]["max_num_results"], 5);
    assert_eq!(b["tools"][1]["type"], "web_search");
    assert_eq!(b["tools"][1]["search_context_size"], "low");
    assert_eq!(b["tools"][2]["container"]["file_ids"][0], "file-x");
    assert_eq!(b["include"][0], "code_interpreter_call.outputs");
    assert_eq!(b["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(b["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(b["input"][0]["content"][1]["file_id"], "file-img");
    assert!(b.get("temperature").is_some());
}

#[test]
fn user_field_fallback_when_not_uuid() {
    assert_eq!(provider_user_field("t", "u"), "t:u");
    assert_eq!(feature_label(&[]), "none");
}

#[test]
#[allow(clippy::cognitive_complexity)] // reason: flat list of event-translation assertions
fn translates_deltas_tools_and_completion() {
    let mut a = OpenAiResponses::default();
    let out = a.translate(&ev(
        "response.output_text.delta",
        &json!({"type":"response.output_text.delta","item_id":"m1","content_index":0,"delta":"Hel"}),
    ));
    assert_eq!(
        out,
        vec![ProviderEvent::Delta {
            kind: DeltaKind::Text,
            text: "Hel".into()
        }]
    );
    // event name taken from data.type when the event line is missing
    let out = a.translate(&SseEvent {
        event: None,
        data: json!({"type":"response.web_search_call.searching"}).to_string(),
    });
    assert!(matches!(&out[0], ProviderEvent::ToolStart { name, .. } if name == "web_search"));
    let out = a.translate(&ev(
        "response.file_search_call.completed",
        &json!({"results":[1,2]}),
    ));
    assert!(matches!(&out[0], ProviderEvent::ToolDone { details, .. } if details["files_searched"] == 2));
    let out = a.translate(&ev(
        "response.output_item.done",
        &json!({"item":{"type":"code_interpreter_call","outputs":[{"type":"logs","logs":"a"},{"type":"logs","logs":"b"}]}}),
    ));
    assert!(matches!(&out[0], ProviderEvent::ToolDone { name, details } if name == "code_interpreter" && details["output"] == "a\nb"));
    let out = a.translate(&ev(
        "response.completed",
        &json!({"response":{"id":"resp_1","usage":{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":1}},
            "output":[{"type":"message","content":[{"type":"output_text","text":"Hello world","annotations":[
                {"type":"url_citation","url":"https://ex.com","title":"Ex","start_index":0,"end_index":5},
                {"type":"file_citation","file_id":"file-abc","filename":"x.pdf","index":3}]}]}]}}),
    ));
    assert_eq!(out.len(), 2);
    match &out[0] {
        ProviderEvent::Citations(c) => {
            assert_eq!(
                c[0],
                RawCitation::Web {
                    url: "https://ex.com".into(),
                    title: "Ex".into(),
                    snippet: "Hello".into(),
                    span: Some((0, 5))
                }
            );
            assert_eq!(c[1], RawCitation::File { file_id: "file-abc".into() });
        }
        other => panic!("unexpected {other:?}"),
    }
    match &out[1] {
        ProviderEvent::Completed {
            response_id,
            usage,
            incomplete_reason,
        } => {
            assert_eq!(response_id.as_deref(), Some("resp_1"));
            let u = usage.unwrap();
            assert_eq!((u.input_tokens, u.output_tokens), (10, 5));
            assert_eq!(u.cache_read_input_tokens, 3);
            assert_eq!(u.reasoning_tokens, 1);
            assert!(incomplete_reason.is_none());
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn translates_incomplete_failed_and_error() {
    let mut a = OpenAiResponses::default();
    let out = a.translate(&ev(
        "response.incomplete",
        &json!({"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":2}}}),
    ));
    assert!(matches!(&out[0], ProviderEvent::Completed { incomplete_reason: Some(r), .. } if r == "max_output_tokens"));
    let out = a.translate(&ev(
        "response.failed",
        &json!({"response":{"error":{"code":"server_error","message":"boom resp_123"},"usage":{"input_tokens":4,"output_tokens":0}}}),
    ));
    match &out[0] {
        ProviderEvent::Failed { message, usage, .. } => {
            assert_eq!(message, "boom resp_123");
            assert_eq!(usage.unwrap().input_tokens, 4);
        }
        other => panic!("unexpected {other:?}"),
    }
    let out = a.translate(&ev("error", &json!({"code":"x","message":"flat"})));
    assert!(matches!(&out[0], ProviderEvent::Failed { message, .. } if message == "flat"));
    let out = a.translate(&SseEvent {
        event: Some("error".into()),
        data: "not json".into(),
    });
    assert!(matches!(&out[0], ProviderEvent::Failed { message, .. } if message == "not json"));
}

#[test]
fn streamed_annotations_are_used_when_completed_has_none() {
    let mut a = OpenAiResponses::default();
    a.translate(&ev(
        "response.output_text.delta",
        &json!({"item_id":"m","content_index":0,"delta":"abcdef"}),
    ));
    a.translate(&ev(
        "response.output_text.annotation.added",
        &json!({"item_id":"m","content_index":0,"annotation":{"type":"url_citation","url":"https://u","title":"T","start_index":1,"end_index":3}}),
    ));
    let out = a.translate(&ev("response.completed", &json!({"response":{"usage":{"input_tokens":1,"output_tokens":1}}})));
    match &out[0] {
        ProviderEvent::Citations(c) => assert!(matches!(&c[0], RawCitation::Web { snippet, .. } if snippet == "bc")),
        other => panic!("unexpected {other:?}"),
    }
}

fn knowledge_request() -> LlmRequest {
    LlmRequest {
        provider_model_id: "m".into(),
        instructions: String::new(),
        input: vec![InputMessage::text(Role::User, "q")],
        max_output_tokens: 10,
        tools: vec![ToolSpec::SearchKnowledge],
        max_tool_calls: 2,
        api_params: ModelApiParams::default(),
        user: "u".into(),
        metadata: RequestMetadata {
            tenant_id: "t".into(),
            user_id: "u".into(),
            chat_id: "c".into(),
            request_type: "chat".into(),
            feature: "none".into(),
        },
        stream: true,
        tool_exchanges: vec![super::ToolExchange {
            call_id: "call_1".into(),
            name: "search_knowledge".into(),
            arguments: "{\"query\":\"vpn\"}".into(),
            output: "{\"results\":[]}".into(),
        }],
    }
}

#[test]
fn function_calls_are_translated_by_every_adapter() {
    // Responses
    let mut a = OpenAiResponses::default();
    let out = a.translate(&ev(
        "response.output_item.done",
        &json!({"item": {"type": "function_call", "call_id": "call_9", "name": "search_knowledge", "arguments": "{\"query\":\"x\"}"}}),
    ));
    assert_eq!(
        out,
        vec![ProviderEvent::FunctionCall {
            call_id: "call_9".into(),
            name: "search_knowledge".into(),
            arguments: "{\"query\":\"x\"}".into()
        }]
    );
    // Chat Completions: streamed tool_calls fragments, start once, done + call at [DONE]
    let mut c = super::chat_completions::ChatCompletions::default();
    let first = c.translate(&SseEvent {
        event: None,
        data: json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_c", "function": {"name": "other_tool", "arguments": "{\"a\""}}]}}]}).to_string(),
    });
    assert!(matches!(&first[..], [ProviderEvent::ToolStart { name, .. }] if name == "function_call"));
    let more = c.translate(&SseEvent {
        event: None,
        data: json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": ":1}"}}]}, "finish_reason": "tool_calls"}]}).to_string(),
    });
    assert!(more.is_empty(), "argument fragments are accumulated: {more:?}");
    let done = c.translate(&SseEvent { event: None, data: "[DONE]".into() });
    assert!(done.iter().any(|e| matches!(e, ProviderEvent::FunctionCall { call_id, name, arguments }
        if call_id == "call_c" && name == "other_tool" && arguments == "{\"a\":1}")));
    assert!(matches!(done.last(), Some(ProviderEvent::Completed { .. })));
    // Anthropic tool_use with input_json_delta
    let mut an = super::anthropic_messages::AnthropicMessages::default();
    an.translate(&ev("content_block_start", &json!({"index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "search_knowledge"}})));
    an.translate(&ev("content_block_delta", &json!({"index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":"}})));
    an.translate(&ev("content_block_delta", &json!({"index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"y\"}"}})));
    let stop = an.translate(&ev("content_block_stop", &json!({"index": 1})));
    assert_eq!(
        stop,
        vec![ProviderEvent::FunctionCall {
            call_id: "toolu_1".into(),
            name: "search_knowledge".into(),
            arguments: "{\"query\":\"y\"}".into()
        }]
    );
}

#[test]
fn tool_exchanges_are_replayed_in_each_adapter_format() {
    let r = knowledge_request();
    let b = OpenAiResponses::default().build_body(&r);
    let input = b["input"].as_array().unwrap();
    assert_eq!(input[1], json!({"type": "function_call", "call_id": "call_1", "name": "search_knowledge", "arguments": "{\"query\":\"vpn\"}"}));
    assert_eq!(input[2], json!({"type": "function_call_output", "call_id": "call_1", "output": "{\"results\":[]}"}));
    assert_eq!(b["tools"][0]["type"], "function");
    assert_eq!(b["tools"][0]["name"], "search_knowledge");
    let b = super::chat_completions::ChatCompletions::default().build_body(&r);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(msgs[2], json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"results\":[]}"}));
    let b = super::anthropic_messages::AnthropicMessages::default().build_body(&r);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
    assert_eq!(msgs[1]["content"][0]["input"], json!({"query": "vpn"}));
    assert_eq!(msgs[2]["content"][0], json!({"type": "tool_result", "tool_use_id": "call_1", "content": "{\"results\":[]}"}));
}
