#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::TokenUsage;
use serde_json::{Value, json};
use uuid::Uuid;

use super::adapter::{EventParser, adapter_for};
use super::alias::{is_ip_host, upstream_alias, upstream_alias_for_tenant};
use crate::config::{ProviderEntry, ProviderKind, TenantOverride};
use crate::domain::llm::{
    ApiParams, Completion, InputMessage, InputRole, LlmEvent, LlmRequest, ProviderErrorKind,
    RawCitation, RequestIdentity, RequestType, ToolSpec,
};

fn request() -> LlmRequest {
    LlmRequest {
        model: "gpt-x".to_owned(),
        system_prompt: "Be brief.".to_owned(),
        input: vec![
            InputMessage {
                role: InputRole::User,
                text: "hi".to_owned(),
                image_file_ids: Vec::new(),
            },
            InputMessage {
                role: InputRole::Assistant,
                text: "hello".to_owned(),
                image_file_ids: Vec::new(),
            },
            InputMessage {
                role: InputRole::User,
                text: "look".to_owned(),
                image_file_ids: vec!["file-img".to_owned()],
            },
        ],
        max_output_tokens: 256,
        tools: Vec::new(),
        max_tool_calls: None,
        identity: RequestIdentity {
            tenant_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            user_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
            chat_id: Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap(),
        },
        request_type: RequestType::Chat,
        api_params: ApiParams::default(),
        feature: "none".to_owned(),
        secondary_file_ids: std::collections::HashMap::new(),
    }
}

fn body_of(kind: ProviderKind, request: &LlmRequest) -> Value {
    adapter_for(kind).build_body(request, true).unwrap()
}

fn feed(parser: &mut dyn EventParser, frames: &[(&str, Value)]) -> Vec<LlmEvent> {
    frames
        .iter()
        .flat_map(|(name, data)| parser.push(Some(name), &data.to_string()))
        .collect()
}

#[test]
fn the_responses_body_carries_identity_metadata_and_the_output_cap() {
    let body = body_of(ProviderKind::OpenaiResponses, &request());

    assert_eq!(body["model"], json!("gpt-x"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["max_output_tokens"], json!(256));
    assert_eq!(body["instructions"], json!("Be brief."));
    assert_eq!(
        body["user"],
        json!(format!("{}{}", "1".repeat(32), "2".repeat(32)))
    );
    assert_eq!(body["metadata"]["request_type"], json!("chat"));
    assert_eq!(body["metadata"]["feature"], json!("none"));
    assert!(body.get("tools").is_none());
    assert!(body.get("temperature").is_none());
    assert_eq!(body["input"][0], json!({ "role": "user", "content": "hi" }));
    assert_eq!(
        body["input"][2]["content"][1],
        json!({ "type": "input_image", "file_id": "file-img" })
    );
}

#[test]
fn the_responses_body_maps_tools_and_includes_code_interpreter_outputs() {
    let mut request = request();
    request.tools = vec![
        ToolSpec::FileSearch {
            vector_store_id: "vs_1".to_owned(),
            max_num_results: 5,
        },
        ToolSpec::WebSearch {
            context_size: "high".to_owned(),
        },
        ToolSpec::CodeInterpreter {
            file_ids: vec!["file-1".to_owned()],
        },
    ];
    request.max_tool_calls = Some(3);

    let body = body_of(ProviderKind::OpenaiResponses, &request);

    assert_eq!(body["tools"][0]["type"], json!("file_search"));
    assert_eq!(body["tools"][0]["vector_store_ids"], json!(["vs_1"]));
    assert_eq!(body["tools"][1]["search_context_size"], json!("high"));
    assert_eq!(body["tools"][2]["container"]["file_ids"], json!(["file-1"]));
    assert_eq!(body["max_tool_calls"], json!(3));
    assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));
}

#[test]
fn api_params_are_sent_only_when_set_and_extra_body_never_overrides_reserved_keys() {
    let mut request = request();
    request.api_params = ApiParams {
        temperature: Some(0.5),
        extra_body: Some(
            json!({ "seed": 7, "model": "evil", "tools": [], "user": "x" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        reasoning_effort: Some("low".to_owned()),
        ..ApiParams::default()
    };

    let body = body_of(ProviderKind::OpenaiResponses, &request);

    assert_eq!(body["temperature"], json!(0.5));
    assert_eq!(body["reasoning"], json!({ "effort": "low" }));
    assert_eq!(body["seed"], json!(7));
    assert_eq!(body["model"], json!("gpt-x"));
    assert!(body.get("tools").is_none());
    assert_ne!(body["user"], json!("x"));
    assert!(body.get("top_p").is_none());
}

#[test]
fn a_text_stream_completes_with_usage_including_cached_and_reasoning_tokens() {
    let mut parser = adapter_for(ProviderKind::OpenaiResponses).parser();
    let events = feed(
        parser.as_mut(),
        &[
            (
                "response.created",
                json!({ "response": { "id": "resp_1" } }),
            ),
            (
                "response.output_text.delta",
                json!({ "delta": "Hel", "item_id": "m", "content_index": 0 }),
            ),
            (
                "response.output_text.delta",
                json!({ "delta": "lo", "item_id": "m", "content_index": 0 }),
            ),
            (
                "response.completed",
                json!({ "response": {
                    "id": "resp_1", "status": "completed", "output": [],
                    "usage": {
                        "input_tokens": 10, "input_tokens_details": { "cached_tokens": 4 },
                        "output_tokens": 5, "output_tokens_details": { "reasoning_tokens": 2 }
                    }
                } }),
            ),
        ],
    );

    assert_eq!(events[0], LlmEvent::TextDelta("Hel".to_owned()));
    assert_eq!(events[1], LlmEvent::TextDelta("lo".to_owned()));
    let LlmEvent::Completed {
        completion,
        response_id,
        usage,
        citations,
    } = &events[2]
    else {
        panic!("a completed event is expected: {events:?}");
    };
    assert_eq!(*completion, Completion::Complete);
    assert_eq!(response_id.as_deref(), Some("resp_1"));
    assert_eq!(
        *usage,
        Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 4,
            cache_write_input_tokens: 0,
            reasoning_tokens: 2,
        })
    );
    assert!(citations.is_empty());
}

#[test]
fn annotations_become_citations_with_the_answer_text_in_range() {
    let annotation = json!({
        "type": "url_citation", "start_index": 0, "end_index": 4,
        "url": "https://example.com/rust", "title": "Rust"
    });
    let file =
        json!({ "type": "file_citation", "file_id": "file-abc", "filename": "a.pdf", "index": 0 });
    let mut parser = adapter_for(ProviderKind::OpenaiResponses).parser();

    let events = feed(
        parser.as_mut(),
        &[(
            "response.completed",
            json!({ "response": { "id": "r", "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "Rust is fun", "annotations": [annotation, file] }]
            }] } }),
        )],
    );

    let LlmEvent::Completed { citations, .. } = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(
        citations[0],
        RawCitation::Web {
            url: "https://example.com/rust".to_owned(),
            title: "Rust".to_owned(),
            snippet: "Rust".to_owned(),
            start: Some(0),
            end: Some(4),
        }
    );
    assert_eq!(
        citations[1],
        RawCitation::File {
            provider_file_id: "file-abc".to_owned(),
            filename: Some("a.pdf".to_owned()),
        }
    );
}

#[test]
fn tool_events_follow_the_translation_table() {
    let mut parser = adapter_for(ProviderKind::OpenaiResponses).parser();

    let events = feed(
        parser.as_mut(),
        &[
            ("response.file_search_call.searching", json!({})),
            (
                "response.file_search_call.completed",
                json!({ "results": [{}, {}] }),
            ),
            ("response.web_search_call.searching", json!({})),
            ("response.web_search_call.completed", json!({})),
            ("response.code_interpreter_call.in_progress", json!({})),
            ("response.code_interpreter_call.interpreting", json!({})),
            ("response.code_interpreter_call.completed", json!({})),
            (
                "response.output_item.done",
                json!({ "item": { "type": "code_interpreter_call",
                    "outputs": [{ "type": "logs", "logs": "a" }, { "type": "logs", "logs": "b" }] } }),
            ),
            (
                "response.output_item.done",
                json!({ "item": { "type": "message" } }),
            ),
        ],
    );

    assert_eq!(
        events,
        vec![
            LlmEvent::ToolStarted {
                name: "file_search".to_owned()
            },
            LlmEvent::ToolDone {
                name: "file_search".to_owned(),
                details: json!({ "files_searched": 2 })
            },
            LlmEvent::ToolStarted {
                name: "web_search".to_owned()
            },
            LlmEvent::ToolDone {
                name: "web_search".to_owned(),
                details: json!({})
            },
            LlmEvent::ToolStarted {
                name: "code_interpreter".to_owned()
            },
            LlmEvent::ToolDone {
                name: "code_interpreter".to_owned(),
                details: json!({ "output": "a\nb" })
            },
        ]
    );
}

#[test]
fn failures_are_read_from_the_response_then_the_top_level_then_the_event_itself() {
    let mut parser = adapter_for(ProviderKind::OpenaiResponses).parser();

    let events = feed(
        parser.as_mut(),
        &[
            (
                "response.failed",
                json!({ "response": { "error": { "message": "in response" }, "usage": { "input_tokens": 3, "output_tokens": 1 } } }),
            ),
            (
                "response.failed",
                json!({ "response": {}, "error": { "message": "top level" } }),
            ),
            (
                "error",
                json!({ "code": "server_error", "message": "flat" }),
            ),
        ],
    );

    let messages: Vec<&str> = events
        .iter()
        .map(|event| match event {
            LlmEvent::Failed { message, .. } => message.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(messages, ["in response", "top level", "flat"]);
    assert!(
        matches!(&events[0], LlmEvent::Failed { usage: Some(usage), .. } if usage.input_tokens == 3)
    );
    assert!(matches!(&events[1], LlmEvent::Failed { usage: None, .. }));
}

#[test]
fn an_incomplete_response_is_a_completion_with_a_normalized_reason() {
    let mut parser = adapter_for(ProviderKind::OpenaiResponses).parser();

    let events = feed(
        parser.as_mut(),
        &[(
            "response.incomplete",
            json!({ "response": { "incomplete_details": { "reason": "max_output_tokens" }, "usage": {} } }),
        )],
    );

    assert!(matches!(
        &events[0],
        LlmEvent::Completed { completion: Completion::Incomplete { reason }, .. } if reason == "max_tokens"
    ));
}

#[test]
fn the_event_name_falls_back_to_the_type_field() {
    let mut parser = adapter_for(ProviderKind::OpenaiResponses).parser();

    let events = parser.push(
        None,
        &json!({ "type": "response.output_text.delta", "delta": "x" }).to_string(),
    );

    assert_eq!(events, vec![LlmEvent::TextDelta("x".to_owned())]);
}

#[test]
fn the_vllm_adapter_drops_tools_and_metadata_and_splits_think_blocks() {
    let mut request = request();
    request.tools = vec![ToolSpec::WebSearch {
        context_size: "low".to_owned(),
    }];
    let body = body_of(ProviderKind::VllmResponses, &request);
    let mut parser = adapter_for(ProviderKind::VllmResponses).parser();

    let events = feed(
        parser.as_mut(),
        &[
            ("response.output_text.delta", json!({ "delta": "<thi" })),
            (
                "response.output_text.delta",
                json!({ "delta": "nk>plan</think>ans" }),
            ),
            ("response.output_text.delta", json!({ "delta": "wer" })),
            ("response.completed", json!({ "response": { "usage": {} } })),
        ],
    );

    assert!(body.get("tools").is_none());
    assert!(body.get("metadata").is_none());
    assert_eq!(events[0], LlmEvent::ReasoningDelta("plan".to_owned()));
    assert_eq!(events[1], LlmEvent::TextDelta("ans".to_owned()));
    assert_eq!(events[2], LlmEvent::TextDelta("wer".to_owned()));
    assert!(matches!(events[3], LlmEvent::Completed { .. }));
}

#[test]
fn the_chat_completions_adapter_builds_messages_and_translates_chunks() {
    let mut request = request();
    request.tools = vec![ToolSpec::WebSearch {
        context_size: "low".to_owned(),
    }];
    let body = body_of(ProviderKind::OpenaiChatCompletions, &request);
    let mut parser = adapter_for(ProviderKind::OpenaiChatCompletions).parser();

    let events = feed(
        parser.as_mut(),
        &[
            (
                "message",
                json!({ "id": "c1", "choices": [{ "delta": { "content": "Hi" } }] }),
            ),
            (
                "message",
                json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
            ),
            (
                "message",
                json!({ "choices": [], "usage": { "prompt_tokens": 7, "completion_tokens": 2,
                "prompt_tokens_details": { "cached_tokens": 1 } } }),
            ),
        ],
    );
    let end = parser.push(None, "[DONE]");

    assert_eq!(
        body["messages"][0],
        json!({ "role": "system", "content": "Be brief." })
    );
    assert_eq!(body["max_completion_tokens"], json!(256));
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));
    assert!(body.get("tools").is_none());
    assert_eq!(events, vec![LlmEvent::TextDelta("Hi".to_owned())]);
    let LlmEvent::Completed {
        usage, response_id, ..
    } = &end[0]
    else {
        panic!("{end:?}");
    };
    assert_eq!(usage.unwrap().input_tokens, 7);
    assert_eq!(usage.unwrap().cache_read_input_tokens, 1);
    assert_eq!(response_id.as_deref(), Some("c1"));
}

#[test]
fn the_anthropic_adapter_maps_server_tools_and_streams() {
    let mut request = request();
    request.tools = vec![
        ToolSpec::FileSearch {
            vector_store_id: "vs".to_owned(),
            max_num_results: 5,
        },
        ToolSpec::WebSearch {
            context_size: "low".to_owned(),
        },
        ToolSpec::CodeInterpreter {
            file_ids: Vec::new(),
        },
    ];
    request.max_tool_calls = Some(2);
    let adapter = adapter_for(ProviderKind::AnthropicMessages);
    let body = adapter.build_body(&request, true).unwrap();
    let mut parser = adapter.parser();

    let events = feed(
        parser.as_mut(),
        &[
            (
                "message_start",
                json!({ "message": { "id": "msg_1", "usage": { "input_tokens": 9 } } }),
            ),
            (
                "content_block_start",
                json!({ "index": 0, "content_block": { "type": "server_tool_use", "name": "web_search" } }),
            ),
            ("content_block_stop", json!({ "index": 0 })),
            (
                "content_block_delta",
                json!({ "index": 1, "delta": { "type": "text_delta", "text": "ok" } }),
            ),
            (
                "content_block_delta",
                json!({ "index": 1, "delta": { "type": "citations_delta",
                "citation": { "type": "web_search_result_location", "url": "https://a.b/c", "title": "T", "cited_text": "x" } } }),
            ),
            (
                "message_delta",
                json!({ "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 4 } }),
            ),
            ("message_stop", json!({})),
        ],
    );

    assert_eq!(body["system"], json!("Be brief."));
    assert_eq!(body["metadata"]["user_id"].as_str().unwrap().len(), 64);
    assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    assert_eq!(body["tools"][0]["type"], json!("web_search_20250305"));
    assert_eq!(body["tools"][1]["type"], json!("code_execution_20250522"));
    assert_eq!(
        adapter.extra_headers(),
        vec![
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "files-api-2025-04-14")
        ]
    );
    assert_eq!(
        events[0],
        LlmEvent::ToolStarted {
            name: "web_search".to_owned()
        }
    );
    assert_eq!(
        events[1],
        LlmEvent::ToolDone {
            name: "web_search".to_owned(),
            details: json!({})
        }
    );
    assert_eq!(events[2], LlmEvent::TextDelta("ok".to_owned()));
    let LlmEvent::Completed {
        usage, citations, ..
    } = &events[3]
    else {
        panic!("{events:?}");
    };
    assert_eq!(
        (usage.unwrap().input_tokens, usage.unwrap().output_tokens),
        (9, 4)
    );
    assert_eq!(citations.len(), 1);
}

#[test]
fn non_streaming_answers_are_read_per_api() {
    let responses = adapter_for(ProviderKind::OpenaiResponses)
        .parse_completion(&json!({
            "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "sum" }] }],
            "usage": { "input_tokens": 3, "output_tokens": 1 }
        }))
        .unwrap();
    let chat = adapter_for(ProviderKind::OpenaiChatCompletions)
        .parse_completion(&json!({ "choices": [{ "message": { "content": "chat" } }] }))
        .unwrap();
    let anthropic = adapter_for(ProviderKind::AnthropicMessages)
        .parse_completion(&json!({ "content": [{ "type": "text", "text": "claude" }] }))
        .unwrap();

    assert_eq!(responses.text, "sum");
    assert_eq!(responses.usage.unwrap().input_tokens, 3);
    assert_eq!(chat.text, "chat");
    assert_eq!(anthropic.text, "claude");
    assert!(
        adapter_for(ProviderKind::OpenaiChatCompletions)
            .parse_completion(&json!({}))
            .is_err()
    );
}

#[test]
fn provider_errors_have_a_code_and_a_retry_hint() {
    let limited = crate::domain::llm::ProviderError::rate_limited(Some(7));

    assert_eq!(limited.kind, ProviderErrorKind::RateLimited);
    assert!(limited.message.contains('7'));
    assert_eq!(limited.kind.code(), "rate_limited");
    assert_eq!(ProviderErrorKind::Timeout.code(), "provider_timeout");
}

fn entry(host: &str, port: Option<u16>, alias: Option<&str>) -> ProviderEntry {
    let mut entry = ProviderEntry::default_openai();
    entry.host = host.to_owned();
    entry.port = port;
    entry.upstream_alias = alias.map(str::to_owned);
    entry
}

#[test]
fn a_hostname_endpoint_routes_by_the_alias_oagw_derives() {
    assert_eq!(upstream_alias(&entry("llm.test", None, None)), "llm.test");
    assert_eq!(
        upstream_alias(&entry("llm.test", Some(8443), None)),
        "llm.test:8443"
    );
    assert_eq!(
        upstream_alias(&entry("LLM.Test.", None, None)),
        "llm.test",
        "lowercase, trailing dot stripped"
    );
    // OAGW rejects an explicit alias other than the derived one for a hostname.
    assert_eq!(
        upstream_alias(&entry("llm.test", None, Some("custom"))),
        "llm.test"
    );
    assert_eq!(
        upstream_alias(&entry("llm.test", Some(8443), Some("custom"))),
        "llm.test:8443"
    );
}

#[test]
fn the_standard_port_depends_on_the_scheme() {
    let mut plain = entry("llm.test", Some(80), None);
    plain.use_http = true;
    assert_eq!(upstream_alias(&plain), "llm.test");
    plain.port = Some(443);
    assert_eq!(
        upstream_alias(&plain),
        "llm.test:443",
        "443 is not standard for HTTP"
    );
    let secure_80 = entry("llm.test", Some(80), None);
    assert_eq!(
        upstream_alias(&secure_80),
        "llm.test:80",
        "80 is not standard for HTTPS"
    );
}

#[test]
fn an_ip_endpoint_routes_by_its_explicit_alias() {
    assert_eq!(
        upstream_alias(&entry("127.0.0.1", Some(8099), None)),
        "127.0.0.1:8099",
        "a non-standard port is part of the default alias"
    );
    assert_eq!(
        upstream_alias(&entry("127.0.0.1", Some(443), None)),
        "127.0.0.1",
        "a standard port is not"
    );
    let mut plain = entry("127.0.0.1", None, None);
    plain.use_http = true;
    assert_eq!(upstream_alias(&plain), "127.0.0.1");
    plain.port = Some(443);
    assert_eq!(upstream_alias(&plain), "127.0.0.1:443");
    assert_eq!(
        upstream_alias(&entry("127.0.0.1", Some(8099), Some("Local-Mock."))),
        "local-mock"
    );
    assert!(is_ip_host("127.0.0.1"));
    assert!(is_ip_host("[::1]"));
    assert!(!is_ip_host("llm.test"));
}

#[test]
fn a_tenant_override_gets_its_own_alias() {
    let provider = entry("127.0.0.1", Some(8099), Some("shared"));
    let host_override = TenantOverride {
        host: Some("tenant.test".to_owned()),
        ..TenantOverride::default()
    };
    let alias_override = TenantOverride {
        upstream_alias: Some("tenant-alias".to_owned()),
        ..TenantOverride::default()
    };
    let hostname_provider = entry("llm.test", None, Some("shared"));

    assert_eq!(upstream_alias_for_tenant(&provider, None), "shared");
    // A hostname override derives its alias.
    assert_eq!(
        upstream_alias_for_tenant(&provider, Some(&host_override)),
        "tenant.test:8099",
        "the override inherits the entry's non-standard port"
    );
    // An IP entry takes the override's explicit alias.
    assert_eq!(
        upstream_alias_for_tenant(&provider, Some(&alias_override)),
        "tenant-alias"
    );
    // A hostname entry cannot register one: the override routes by the derived alias.
    assert_eq!(
        upstream_alias_for_tenant(&hostname_provider, Some(&alias_override)),
        "llm.test"
    );
}

#[test]
fn the_anthropic_adapter_sends_images_by_their_secondary_copy_and_drops_the_others() {
    let mut with_copy = request();
    with_copy
        .secondary_file_ids
        .insert("file-img".to_owned(), "file_011anthropic".to_owned());
    let without_copy = request();

    let sent = body_of(ProviderKind::AnthropicMessages, &with_copy);
    let dropped = body_of(ProviderKind::AnthropicMessages, &without_copy);

    assert_eq!(
        sent["messages"][2]["content"],
        json!([
            { "type": "text", "text": "look" },
            { "type": "image", "source": { "type": "file", "file_id": "file_011anthropic" } }
        ])
    );
    assert_eq!(dropped["messages"][2]["content"], json!("look"));
    assert_eq!(sent["messages"][0]["content"], json!("hi"));
}
