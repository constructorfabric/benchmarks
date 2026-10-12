use std::sync::Arc;

use futures::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode};
use mini_chat_sdk::ModelApiParams;
use oagw_sdk::api::ErrorSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    CODE_OUTPUT_MAX_CHARS, OpenAiResponsesAdapter, ResponsesTranslator, TRUNCATED_SUFFIX,
    build_request_body, map_gateway_error, map_http_error, parse_completion, request_uri,
    truncate_output,
};
use crate::config::ProviderKind;
use crate::domain::llm::{
    LlmAnnotation, LlmAnnotationKind, LlmEvent, LlmFailure, LlmFailureKind, LlmFunctionCall,
    LlmFunctionTool, LlmInputItem, LlmMetadata, LlmProvider, LlmRequest, LlmRequestType, LlmTool,
    LlmUsage, ProviderTarget,
};
use crate::infra::llm::fake_gateway::{
    FakeGateway, StaticSystemContext, deadline_exceeded, json_response, lock, sse_response,
};
use crate::infra::llm::sse::{SseParser, SseTranslator};
use crate::infra::oagw_provisioning::SystemContextError;

const RAW_MESSAGE: &str = "Upstream failure for response resp_abc123def456ghi789 (msg_ZyXw9876543210) \
    while reading file-AbCdEf123456789012 from vs_QwErTy1234567890 \
    with key sk-AbCdEfGhIjKlMnOp0123 at https://internal-llm.example.com/v1/responses \
    and file-based file_search was skipped";
const SANITIZED_MESSAGE: &str = "Upstream failure for response [provider_id] ([provider_id]) \
    while reading [provider_id] from [provider_id] \
    with key [credential] at [url] \
    and file-based file_search was skipped";

// ── Request building ─────────────────────────────────────────────────────

fn request(tools: Vec<LlmTool>, api_params: ModelApiParams) -> LlmRequest {
    LlmRequest {
        model: "gpt-4.1-2025-04-14".to_owned(),
        instructions: "Be brief.".to_owned(),
        input: vec![
            LlmInputItem::User {
                text: "first".to_owned(),
                image_file_ids: vec![],
            },
            LlmInputItem::Assistant {
                text: "answer".to_owned(),
            },
            LlmInputItem::User {
                text: "What is in this image?".to_owned(),
                image_file_ids: vec!["file-abc123".to_owned()],
            },
        ],
        tools,
        max_output_tokens: 4096,
        max_tool_calls: 2,
        api_params,
        user: "u".repeat(64),
        metadata: LlmMetadata {
            tenant_id: "t".to_owned(),
            user_id: "u".to_owned(),
            chat_id: "c".to_owned(),
            request_type: LlmRequestType::Chat,
            feature: "none".to_owned(),
        },
        secondary_file_ids: std::collections::BTreeMap::new(),
    }
}

fn all_tools() -> Vec<LlmTool> {
    vec![
        LlmTool::FileSearch {
            vector_store_id: "vs_1".to_owned(),
            max_num_results: 5,
        },
        LlmTool::WebSearch {
            search_context_size: "low".to_owned(),
        },
        LlmTool::CodeInterpreter {
            file_ids: vec!["file-x".to_owned()],
        },
    ]
}

#[test]
fn request_body_carries_input_identity_and_limits() {
    let body = build_request_body(&request(vec![], ModelApiParams::default()), true);
    assert_eq!(
        body,
        json!({
            "model": "gpt-4.1-2025-04-14",
            "instructions": "Be brief.",
            "input": [
                { "role": "user", "content": [{ "type": "input_text", "text": "first" }] },
                { "role": "assistant", "content": [{ "type": "output_text", "text": "answer" }] },
                { "role": "user", "content": [
                    { "type": "input_text", "text": "What is in this image?" },
                    { "type": "input_image", "file_id": "file-abc123" },
                ] },
            ],
            "stream": true,
            "max_output_tokens": 4096,
            "max_tool_calls": 2,
            "user": "u".repeat(64),
            "metadata": {
                "tenant_id": "t",
                "user_id": "u",
                "chat_id": "c",
                "request_type": "chat",
                "feature": "none",
            },
        })
    );
}

#[test]
fn request_body_tools_and_include() {
    let body = build_request_body(&request(all_tools(), ModelApiParams::default()), false);
    assert_eq!(body["stream"], json!(false));
    assert_eq!(
        body["tools"],
        json!([
            { "type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 5 },
            { "type": "web_search", "search_context_size": "low" },
            { "type": "code_interpreter", "container": { "type": "auto", "file_ids": ["file-x"] } },
        ])
    );
    assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));

    let web_only = vec![LlmTool::WebSearch {
        search_context_size: "high".to_owned(),
    }];
    let body = build_request_body(&request(web_only, ModelApiParams::default()), true);
    assert!(body.get("include").is_none());
}

#[test]
fn request_body_sends_function_tools() {
    let tools = vec![LlmTool::Function(LlmFunctionTool {
        name: "search_knowledge".to_owned(),
        description: "Search".to_owned(),
        parameters: json!({ "type": "object" }),
    })];
    let body = build_request_body(&request(tools, ModelApiParams::default()), true);
    assert_eq!(
        body["tools"],
        json!([{ "type": "function", "name": "search_knowledge", "description": "Search",
                 "parameters": { "type": "object" } }])
    );
}

#[test]
fn agentic_input_sends_function_calls_and_outputs() {
    let mut req = request(vec![], ModelApiParams::default());
    req.input.extend([
        LlmInputItem::Assistant {
            text: "Let me check.".to_owned(),
        },
        LlmInputItem::FunctionCall(LlmFunctionCall {
            call_id: "call_1".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: r#"{"query":"revenue"}"#.to_owned(),
        }),
        LlmInputItem::FunctionCall(LlmFunctionCall {
            call_id: "call_2".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: "not json".to_owned(),
        }),
        LlmInputItem::FunctionCallOutput {
            call_id: "call_1".to_owned(),
            output: r#"{"results":[]}"#.to_owned(),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_2".to_owned(),
            output: "search limit reached".to_owned(),
        },
    ]);
    let body = build_request_body(&req, true);
    let input = body["input"].as_array().unwrap();
    assert_eq!(
        input[input.len() - 5..],
        [
            json!({ "role": "assistant", "content": [{ "type": "output_text", "text": "Let me check." }] }),
            json!({ "type": "function_call", "call_id": "call_1", "name": "search_knowledge",
                    "arguments": r#"{"query":"revenue"}"# }),
            json!({ "type": "function_call", "call_id": "call_2", "name": "search_knowledge",
                    "arguments": "not json" }),
            json!({ "type": "function_call_output", "call_id": "call_1", "output": r#"{"results":[]}"# }),
            json!({ "type": "function_call_output", "call_id": "call_2", "output": "search limit reached" }),
        ]
    );
}

#[test]
fn function_call_item_is_a_function_call_event_without_tool_events() {
    let item = json!({ "type": "function_call", "id": "fc_1", "call_id": "call_9",
                       "name": "search_knowledge", "arguments": "{\"query\":\"x\"}" });
    let wire = sse(
        "response.output_item.done",
        &json!({ "output_index": 0, "item": item }),
    );
    assert_eq!(
        translate_wire(&wire),
        vec![LlmEvent::FunctionCall(LlmFunctionCall {
            call_id: "call_9".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: "{\"query\":\"x\"}".to_owned(),
        })]
    );
}

#[test]
fn request_body_api_params_and_extra_body_filtering() {
    let extra = json!({
        "service_tier": "flex",
        "temperature": 0.9,
        "model": "evil",
        "stream": false,
        "user": "evil",
        "metadata": {},
        "tools": [],
    });
    let params = ModelApiParams {
        temperature: Some(0.2),
        top_p: Some(0.5),
        frequency_penalty: None,
        presence_penalty: Some(0.1),
        stop: vec!["END".to_owned()],
        extra_body: extra.as_object().cloned(),
        reasoning_effort: Some("low".to_owned()),
    };
    let mut req = request(vec![], params);
    req.instructions = String::new();
    let body = build_request_body(&req, true);

    assert_eq!(body["service_tier"], json!("flex"));
    assert_eq!(
        body["temperature"],
        json!(0.2),
        "api_params win over extra_body"
    );
    assert_eq!(body["top_p"], json!(0.5));
    assert_eq!(body["presence_penalty"], json!(0.1));
    assert!(
        body.get("frequency_penalty").is_none(),
        "unset sampling params are omitted"
    );
    assert_eq!(body["stop"], json!(["END"]));
    assert_eq!(body["reasoning"], json!({ "effort": "low" }));
    assert_eq!(body["model"], json!("gpt-4.1-2025-04-14"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["user"], json!("u".repeat(64)));
    assert_eq!(body["metadata"]["request_type"], json!("chat"));
    assert!(body.get("tools").is_none());
    assert!(
        body.get("instructions").is_none(),
        "empty instructions are omitted"
    );
}

#[test]
fn request_uri_replaces_the_model_placeholder() {
    assert_eq!(
        request_uri("api.openai.com", "/v1/responses", "gpt-4.1"),
        "/api.openai.com/v1/responses"
    );
    assert_eq!(
        request_uri(
            "res.openai.azure.com",
            "/openai/deployments/{model}/chat/completions?api-version=2024-10-21",
            "my deploy/1"
        ),
        "/res.openai.azure.com/openai/deployments/my%20deploy%2F1/chat/completions?api-version=2024-10-21"
    );
}

// ── Stream translation ───────────────────────────────────────────────────

fn sse(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn usage_json() -> Value {
    json!({
        "input_tokens": 120,
        "input_tokens_details": { "cached_tokens": 20 },
        "output_tokens": 30,
        "output_tokens_details": { "reasoning_tokens": 5 },
        "total_tokens": 150,
    })
}

fn url_annotation() -> Value {
    json!({ "type": "url_citation", "url": "https://example.com/a", "title": "A", "start_index": 6, "end_index": 11 })
}

fn file_annotation() -> Value {
    json!({ "type": "file_citation", "file_id": "file-AbCdEf123456789012", "filename": "doc.pdf", "index": 11 })
}

/// A recorded completed stream in the mock provider's wire format.
fn completed_wire() -> String {
    let message = json!({
        "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
        "content": [{ "type": "output_text", "text": "Hello world", "annotations": [url_annotation(), file_annotation()] }],
    });
    let ci_item = json!({
        "type": "code_interpreter_call", "id": "ci_1", "status": "completed",
        "outputs": [{ "type": "logs", "logs": "4" }, { "type": "image", "url": "x" }, { "type": "logs", "logs": "2" }],
    });
    [
        sse("response.created", &json!({ "type": "response.created", "response": { "id": "resp_1", "status": "in_progress" } })),
        sse("response.web_search_call.searching", &json!({ "item_id": "ws_1" })),
        sse("response.web_search_call.completed", &json!({ "item_id": "ws_1" })),
        sse("response.file_search_call.searching", &json!({ "item_id": "fs_1" })),
        sse("response.file_search_call.completed", &json!({ "item_id": "fs_1", "results": [{}, {}, {}] })),
        sse("response.code_interpreter_call.in_progress", &json!({ "item_id": "ci_1" })),
        sse("response.code_interpreter_call.interpreting", &json!({ "item_id": "ci_1" })),
        sse("response.code_interpreter_call.completed", &json!({ "item_id": "ci_1" })),
        sse("response.output_item.done", &json!({ "item": ci_item })),
        sse("response.output_text.delta", &json!({ "item_id": "msg_1", "content_index": 0, "delta": "Hello" })),
        sse("response.output_text.delta", &json!({ "item_id": "msg_1", "content_index": 0, "delta": " world" })),
        sse("response.output_text.annotation.added", &json!({ "item_id": "msg_1", "content_index": 0, "annotation": url_annotation() })),
        sse("response.output_item.done", &json!({ "item": message })),
        // Event name taken from `type` when the `event:` line is missing.
        format!(
            "data: {}\n\n",
            json!({ "type": "response.completed", "response": {
                "id": "resp_1", "status": "completed", "output": [message], "usage": usage_json(),
            } })
        ),
    ]
    .concat()
}

fn web_annotation() -> LlmAnnotation {
    LlmAnnotation {
        kind: LlmAnnotationKind::Url,
        url: Some("https://example.com/a".to_owned()),
        title: Some("A".to_owned()),
        file_id: None,
        filename: None,
        start: Some(6),
        end: Some(11),
        snippet: "world".to_owned(),
    }
}

fn expected_completed_events() -> Vec<LlmEvent> {
    vec![
        LlmEvent::ToolStarted {
            name: "web_search".to_owned(),
            details: json!({}),
        },
        LlmEvent::ToolCompleted {
            name: "web_search".to_owned(),
            details: json!({}),
        },
        LlmEvent::ToolStarted {
            name: "file_search".to_owned(),
            details: json!({}),
        },
        LlmEvent::ToolCompleted {
            name: "file_search".to_owned(),
            details: json!({ "files_searched": 3 }),
        },
        LlmEvent::ToolStarted {
            name: "code_interpreter".to_owned(),
            details: json!({}),
        },
        LlmEvent::ToolCompleted {
            name: "code_interpreter".to_owned(),
            details: json!({ "output": "4\n2" }),
        },
        LlmEvent::TextDelta("Hello".to_owned()),
        LlmEvent::TextDelta(" world".to_owned()),
        LlmEvent::Annotation(web_annotation()),
        // Only in the completed response: emitted once, before the terminal.
        LlmEvent::Annotation(LlmAnnotation {
            kind: LlmAnnotationKind::File,
            url: None,
            title: None,
            file_id: Some("file-AbCdEf123456789012".to_owned()),
            filename: Some("doc.pdf".to_owned()),
            start: None,
            end: None,
            snippet: String::new(),
        }),
        LlmEvent::Completed {
            response_id: Some("resp_1".to_owned()),
            usage: Some(LlmUsage {
                input_tokens: 120,
                output_tokens: 30,
                cache_read_input_tokens: 20,
                cache_write_input_tokens: 0,
                reasoning_tokens: 5,
            }),
            incomplete_reason: None,
            output_text: "Hello world".to_owned(),
        },
    ]
}

/// Translate a wire split into the given chunks.
fn translate_chunks(chunks: &[&[u8]]) -> Vec<LlmEvent> {
    let mut parser = SseParser::default();
    let mut translator = ResponsesTranslator::default();
    let mut out = Vec::new();
    for chunk in chunks {
        for event in parser.push(chunk) {
            translator.translate(event, &mut out);
        }
    }
    for event in parser.finish() {
        translator.translate(event, &mut out);
    }
    out
}

fn translate_wire(wire: &str) -> Vec<LlmEvent> {
    translate_chunks(&[wire.as_bytes()])
}

#[test]
fn completed_stream_translation() {
    assert_eq!(
        translate_wire(&completed_wire()),
        expected_completed_events()
    );
}

#[test]
fn translation_is_independent_of_chunk_boundaries() {
    let wire = completed_wire();
    let bytes = wire.as_bytes();
    for split in (0..bytes.len()).step_by(7) {
        let (head, tail) = bytes.split_at(split);
        assert_eq!(
            translate_chunks(&[head, tail]),
            expected_completed_events(),
            "split {split}"
        );
    }
    let small: Vec<&[u8]> = bytes.chunks(13).collect();
    assert_eq!(translate_chunks(&small), expected_completed_events());
}

#[test]
fn incomplete_response_completes_with_reason() {
    let wire = [
        sse(
            "response.output_text.delta",
            &json!({ "item_id": "m", "delta": "partial" }),
        ),
        sse(
            "response.incomplete",
            &json!({ "response": {
            "id": "resp_2", "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" }, "usage": usage_json(),
        } }),
        ),
    ]
    .concat();
    let events = translate_wire(&wire);
    assert_eq!(events.len(), 2);
    let LlmEvent::Completed {
        incomplete_reason,
        output_text,
        usage,
        ..
    } = &events[1]
    else {
        panic!("expected Completed, got {:?}", events[1]);
    };
    assert_eq!(incomplete_reason.as_deref(), Some("max_output_tokens"));
    assert_eq!(
        output_text, "partial",
        "deltas are used when the response has no output"
    );
    assert_eq!(usage.map(|u| u.output_tokens), Some(30));
}

fn single_failure(wire: &str) -> LlmFailure {
    let events = translate_wire(wire);
    match events.as_slice() {
        [LlmEvent::Failed(failure)] => failure.clone(),
        other => panic!("expected one failure, got {other:?}"),
    }
}

#[test]
fn response_failed_is_sanitized_provider_error_with_usage() {
    let failure = single_failure(&sse(
        "response.failed",
        &json!({ "response": {
            "id": "resp_3", "status": "failed", "usage": usage_json(),
            "error": { "code": "server_error", "message": RAW_MESSAGE },
        } }),
    ));
    assert_eq!(failure.kind, LlmFailureKind::ProviderError);
    assert_eq!(failure.message_sanitized, SANITIZED_MESSAGE);
    assert_eq!(failure.provider_code.as_deref(), Some("server_error"));
    assert_eq!(failure.usage.map(|u| u.input_tokens), Some(120));
}

#[test]
fn response_failed_falls_back_to_top_level_error() {
    let failure = single_failure(&sse(
        "response.failed",
        &json!({ "response": { "status": "failed", "error": null }, "error": { "message": "top level" } }),
    ));
    assert_eq!(failure.message_sanitized, "top level");
    assert_eq!(failure.usage, None);

    let failure = single_failure(&sse("response.failed", &json!({ "response": {} })));
    assert_eq!(failure.message_sanitized, "Provider response failed");
}

#[test]
fn sse_error_event_variants() {
    // Flat {code, message} (mock `sse_error`).
    let failure = single_failure(&sse(
        "error",
        &json!({ "type": "error", "code": "server_error", "message": RAW_MESSAGE, "param": null }),
    ));
    assert_eq!(failure.message_sanitized, SANITIZED_MESSAGE);
    assert_eq!(failure.provider_code.as_deref(), Some("server_error"));

    // Parsed like `response.failed` first.
    let failure = single_failure(&sse(
        "error",
        &json!({ "response": { "error": { "message": "nested resp_abc1" } } }),
    ));
    assert_eq!(failure.message_sanitized, "nested [provider_id]");

    // Unparseable data becomes the message.
    let failure = single_failure("event: error\ndata: overloaded, see https://status.example\n\n");
    assert_eq!(failure.message_sanitized, "overloaded, see [url]");

    // Name from `type` when the event line is `message`.
    let failure =
        single_failure("event: message\ndata: {\"type\":\"error\",\"message\":\"m\"}\n\n");
    assert_eq!(failure.message_sanitized, "m");
}

#[test]
fn unknown_and_ignored_events_produce_nothing() {
    let wire = [
        sse("response.in_progress", &json!({})),
        sse("response.content_part.added", &json!({})),
        sse(
            "response.output_item.done",
            &json!({ "item": { "type": "web_search_call" } }),
        ),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat();
    assert!(translate_wire(&wire).is_empty());
}

#[test]
fn annotation_snippets_use_the_part_text_range() {
    let annotated = |annotation: Value| {
        [
            sse(
                "response.output_text.delta",
                &json!({ "item_id": "m", "content_index": 0, "delta": "abc" }),
            ),
            sse(
                "response.output_text.annotation.added",
                &json!({ "item_id": "m", "content_index": 0, "annotation": annotation }),
            ),
        ]
        .concat()
    };
    let snippet = |wire: &str| match translate_wire(wire).last() {
        Some(LlmEvent::Annotation(annotation)) => annotation.snippet.clone(),
        other => panic!("expected an annotation, got {other:?}"),
    };
    let in_range = json!({ "type": "url_citation", "url": "u", "start_index": 1, "end_index": 3 });
    assert_eq!(snippet(&annotated(in_range)), "bc");
    let out_of_range =
        json!({ "type": "url_citation", "url": "u", "start_index": 1, "end_index": 9 });
    assert_eq!(snippet(&annotated(out_of_range)), "");
    let own_text = json!({ "type": "url_citation", "url": "u", "start_index": 0, "end_index": 1, "text": "quoted" });
    assert_eq!(snippet(&annotated(own_text)), "quoted");
    let unknown = json!({ "type": "container_file_citation", "file_id": "f" });
    assert_eq!(
        translate_wire(&annotated(unknown)).len(),
        1,
        "only the delta"
    );
}

#[test]
fn code_interpreter_output_is_truncated() {
    let short = "x".repeat(CODE_OUTPUT_MAX_CHARS);
    assert_eq!(truncate_output(&short), short);
    let long = "\u{e9}".repeat(CODE_OUTPUT_MAX_CHARS + 1);
    let truncated = truncate_output(&long);
    assert!(truncated.ends_with(TRUNCATED_SUFFIX));
    assert_eq!(
        truncated.chars().count(),
        CODE_OUTPUT_MAX_CHARS + TRUNCATED_SUFFIX.len()
    );

    let item = json!({ "item": { "type": "code_interpreter_call", "outputs": [{ "type": "logs", "logs": "y".repeat(9000) }] } });
    let events = translate_wire(&sse("response.output_item.done", &item));
    let LlmEvent::ToolCompleted { details, .. } = &events[0] else {
        panic!("expected ToolCompleted");
    };
    let output = details["output"].as_str().unwrap();
    assert_eq!(output.len(), CODE_OUTPUT_MAX_CHARS + TRUNCATED_SUFFIX.len());
}

// ── HTTP and gateway errors ──────────────────────────────────────────────

fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(*name, HeaderValue::from_static(value));
    }
    map
}

fn error_body(message: &str) -> Vec<u8> {
    serde_json::to_vec(
        &json!({ "error": { "message": message, "type": "server_error", "code": "server_error" } }),
    )
    .unwrap()
}

#[test]
fn http_429_is_rate_limited_with_retry_after() {
    let failure = map_http_error(
        StatusCode::TOO_MANY_REQUESTS,
        &headers(&[("retry-after", "7")]),
        Some(ErrorSource::Upstream),
        &error_body(RAW_MESSAGE),
    );
    assert_eq!(failure.kind, LlmFailureKind::RateLimited);
    assert_eq!(failure.retry_after_secs, Some(7));
    assert!(
        failure.message_sanitized.contains('7'),
        "{}",
        failure.message_sanitized
    );
    assert!(!failure.message_sanitized.contains("resp_"));

    let failure = map_http_error(
        StatusCode::TOO_MANY_REQUESTS,
        &headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]),
        None,
        b"",
    );
    assert_eq!(failure.kind, LlmFailureKind::RateLimited);
    assert_eq!(failure.retry_after_secs, None);
}

#[test]
fn gateway_504_is_timeout_but_provider_504_is_provider_error() {
    let problem = serde_json::to_vec(&json!({
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~",
        "title": "Deadline Exceeded", "status": 504, "detail": "request timed out",
    }))
    .unwrap();
    let by_source = map_http_error(
        StatusCode::GATEWAY_TIMEOUT,
        &HeaderMap::new(),
        Some(ErrorSource::Gateway),
        b"{}",
    );
    assert_eq!(by_source.kind, LlmFailureKind::ProviderTimeout);
    let by_problem = map_http_error(
        StatusCode::GATEWAY_TIMEOUT,
        &HeaderMap::new(),
        None,
        &problem,
    );
    assert_eq!(by_problem.kind, LlmFailureKind::ProviderTimeout);

    let provider = map_http_error(
        StatusCode::GATEWAY_TIMEOUT,
        &HeaderMap::new(),
        Some(ErrorSource::Upstream),
        &error_body("upstream model timed out"),
    );
    assert_eq!(provider.kind, LlmFailureKind::ProviderError);
    assert_eq!(provider.message_sanitized, "upstream model timed out");
}

#[test]
fn other_http_errors_are_sanitized_provider_errors() {
    let failure = map_http_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        &HeaderMap::new(),
        None,
        &error_body(RAW_MESSAGE),
    );
    assert_eq!(failure.kind, LlmFailureKind::ProviderError);
    assert_eq!(failure.message_sanitized, SANITIZED_MESSAGE);
    assert_eq!(failure.provider_code.as_deref(), Some("server_error"));

    let html = map_http_error(
        StatusCode::BAD_GATEWAY,
        &HeaderMap::new(),
        None,
        b"<html>bad</html>",
    );
    assert_eq!(html.message_sanitized, "Provider returned HTTP 502");

    let flat =
        serde_json::to_vec(&json!({ "message": "flat sk-AbCdEfGhIjKl", "code": 400 })).unwrap();
    let flat = map_http_error(StatusCode::BAD_REQUEST, &HeaderMap::new(), None, &flat);
    assert_eq!(flat.message_sanitized, "flat [credential]");
    assert_eq!(flat.provider_code.as_deref(), Some("400"));
}

#[test]
fn gateway_errors_map_to_timeout_or_provider_error() {
    let timeout = map_gateway_error("openai", deadline_exceeded());
    assert_eq!(timeout.kind, LlmFailureKind::ProviderTimeout);

    let unavailable = map_gateway_error(
        "openai",
        toolkit_canonical_errors::CanonicalError::service_unavailable().create(),
    );
    assert_eq!(unavailable.kind, LlmFailureKind::ProviderError);
    assert_eq!(
        unavailable.message_sanitized,
        "Provider is currently unavailable"
    );
}

// ── Non-streaming responses ──────────────────────────────────────────────

#[test]
fn completion_parsing() {
    let completed = json!({
        "id": "resp_9", "status": "completed", "usage": usage_json(),
        "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "<summary>s</summary>" }] }],
    });
    let completion = parse_completion(&serde_json::to_vec(&completed).unwrap()).unwrap();
    assert_eq!(completion.text, "<summary>s</summary>");
    assert_eq!(completion.response_id.as_deref(), Some("resp_9"));
    assert_eq!(completion.usage.map(|u| u.input_tokens), Some(120));
    assert_eq!(completion.incomplete_reason, None);

    let failed =
        json!({ "status": "failed", "error": { "code": "server_error", "message": RAW_MESSAGE } });
    let failure = parse_completion(&serde_json::to_vec(&failed).unwrap()).unwrap_err();
    assert_eq!(failure.message_sanitized, SANITIZED_MESSAGE);

    assert!(parse_completion(b"not json").is_err());
}

// ── Adapter over a fake gateway ──────────────────────────────────────────

fn target() -> ProviderTarget {
    ProviderTarget {
        provider_id: "openai".to_owned(),
        kind: ProviderKind::OpenaiResponses,
        alias: "127.0.0.1".to_owned(),
        api_path: "/v1/responses".to_owned(),
        tenant_id: Uuid::from_u128(7),
        tenant_override: false,
    }
}

fn adapter(gateway: &Arc<FakeGateway>) -> OpenAiResponsesAdapter {
    OpenAiResponsesAdapter::new(gateway.clone(), Arc::new(StaticSystemContext::default()))
}

#[tokio::test]
async fn stream_sends_through_oagw_and_translates() {
    let gateway = Arc::new(FakeGateway::default());
    let wire = completed_wire();
    let (head, tail) = wire.split_at(wire.len().div_ceil(3));
    gateway.push_proxy(Ok(sse_response(vec![head.to_owned(), tail.to_owned()])));

    let stream = adapter(&gateway)
        .stream(
            &target(),
            request(vec![], ModelApiParams::default()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let events: Vec<LlmEvent> = stream.collect().await;
    assert_eq!(events, expected_completed_events());

    let proxied = lock(&gateway.proxied).clone();
    assert_eq!(proxied.len(), 1);
    let sent = &proxied[0];
    assert_eq!(sent.method, http::Method::POST);
    assert_eq!(sent.uri, "/127.0.0.1/v1/responses");
    assert_eq!(sent.headers["content-type"], "application/json");
    assert_eq!(sent.body["stream"], json!(true));
    assert_eq!(
        sent.tenant_id,
        Uuid::from_u128(0x2222),
        "S2S context is used"
    );
}

#[tokio::test]
async fn stream_http_errors_are_returned_before_streaming() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        429,
        &json!({ "error": { "message": RAW_MESSAGE } }),
        &[("retry-after", "7")],
        Some(ErrorSource::Upstream),
    )));
    gateway.push_proxy(Err(deadline_exceeded()));

    let adapter = adapter(&gateway);
    let req = || request(vec![], ModelApiParams::default());
    let Err(failure) = adapter
        .stream(&target(), req(), CancellationToken::new())
        .await
    else {
        panic!("expected a failure");
    };
    assert_eq!(failure.kind, LlmFailureKind::RateLimited);
    assert_eq!(failure.retry_after_secs, Some(7));
    let Err(failure) = adapter
        .stream(&target(), req(), CancellationToken::new())
        .await
    else {
        panic!("expected a failure");
    };
    assert_eq!(failure.kind, LlmFailureKind::ProviderTimeout);
}

#[tokio::test]
async fn stream_dropped_without_terminal_is_provider_error() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(sse_response(vec![sse(
        "response.output_text.delta",
        &json!({ "delta": "one " }),
    )])));
    let events: Vec<LlmEvent> = adapter(&gateway)
        .stream(
            &target(),
            request(vec![], ModelApiParams::default()),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("one ".to_owned()),
            LlmEvent::Failed(LlmFailure::provider_error(
                "provider stream ended unexpectedly"
            )),
        ]
    );
}

#[tokio::test]
async fn s2s_context_failure_is_not_sent_and_never_proxied() {
    let gateway = Arc::new(FakeGateway::default());
    let system = Arc::new(StaticSystemContext::default());
    *lock(&system.fail_with) = Some(SystemContextError::Unavailable("idp down".to_owned()));
    let adapter = OpenAiResponsesAdapter::new(gateway.clone(), system);

    let Err(failure) = adapter
        .stream(
            &target(),
            request(vec![], ModelApiParams::default()),
            CancellationToken::new(),
        )
        .await
    else {
        panic!("expected a failure");
    };
    assert_eq!(failure.kind, LlmFailureKind::ProviderError);
    assert!(!failure.request_sent, "{failure:?}");
    assert!(
        !failure.message_sanitized.contains("idp down"),
        "{failure:?}"
    );
    let completion = adapter
        .complete(&target(), request(vec![], ModelApiParams::default()))
        .await
        .unwrap_err();
    assert!(!completion.request_sent, "{completion:?}");
    assert!(lock(&gateway.proxied).is_empty(), "no proxy_request call");
}

#[tokio::test]
async fn failures_after_the_outbound_call_are_marked_sent() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        500,
        &json!({ "error": { "message": "boom" } }),
        &[],
        Some(ErrorSource::Upstream),
    )));
    gateway.push_proxy(Err(deadline_exceeded()));
    let adapter = adapter(&gateway);
    for _ in 0..2 {
        let Err(failure) = adapter
            .stream(
                &target(),
                request(vec![], ModelApiParams::default()),
                CancellationToken::new(),
            )
            .await
        else {
            panic!("expected a failure");
        };
        assert!(failure.request_sent, "{failure:?}");
    }
    assert_eq!(lock(&gateway.proxied).len(), 2);
}

#[tokio::test]
async fn cancelled_before_the_request_returns_a_failure() {
    let gateway = Arc::new(FakeGateway::default());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = adapter(&gateway)
        .stream(
            &target(),
            request(vec![], ModelApiParams::default()),
            cancel,
        )
        .await;
    assert!(result.is_err());
    assert!(lock(&gateway.proxied).is_empty());
}

#[tokio::test]
async fn complete_sends_a_non_streaming_request() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        200,
        &json!({ "id": "resp_s", "status": "completed",
                 "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "summary" }] }] }),
        &[],
        None,
    )));
    let mut req = request(vec![], ModelApiParams::default());
    req.metadata.request_type = LlmRequestType::Summary;
    let completion = adapter(&gateway).complete(&target(), req).await.unwrap();
    assert_eq!(completion.text, "summary");
    let sent = lock(&gateway.proxied)[0].clone();
    assert_eq!(sent.body["stream"], json!(false));
    assert_eq!(sent.body["metadata"]["request_type"], json!("summary"));
    assert_eq!(sent.headers["accept"], "application/json");
}
