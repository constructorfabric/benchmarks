#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use mini_chat_sdk::ModelApiParams;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ThinkSegment, ThinkSplitter, VllmResponsesAdapter, build_request_body, strip_think};
use crate::config::ProviderKind;
use crate::domain::llm::{
    LlmEvent, LlmFunctionTool, LlmInputItem, LlmMetadata, LlmProvider, LlmRequest, LlmRequestType,
    LlmTool, LlmUsage, ProviderTarget,
};
use crate::infra::llm::fake_gateway::{
    FakeGateway, StaticSystemContext, json_response, lock, sse_response,
};
use crate::infra::llm::openai_responses::ResponsesTranslator;
use crate::infra::llm::sse::{SseParser, SseTranslator};

fn request(tools: Vec<LlmTool>, api_params: ModelApiParams) -> LlmRequest {
    LlmRequest {
        model: "qwen3-32b-awq".to_owned(),
        instructions: "Be brief.".to_owned(),
        input: vec![LlmInputItem::User {
            text: "What is the answer?".to_owned(),
            image_file_ids: vec![],
        }],
        tools,
        max_output_tokens: 2048,
        max_tool_calls: 2,
        api_params,
        user: "u".repeat(64),
        metadata: LlmMetadata {
            tenant_id: "t".to_owned(),
            user_id: "u".to_owned(),
            chat_id: "c".to_owned(),
            request_type: LlmRequestType::Chat,
            feature: "web_search".to_owned(),
        },
        secondary_file_ids: BTreeMap::new(),
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
        LlmTool::Function(LlmFunctionTool {
            name: "search_knowledge".to_owned(),
            description: "Search".to_owned(),
            parameters: json!({}),
        }),
    ]
}

// ── Request building ─────────────────────────────────────────────────────

#[test]
fn request_body_has_no_tools_metadata_include_or_max_tool_calls() {
    let body = build_request_body(&request(all_tools(), ModelApiParams::default()), true);
    assert_eq!(
        body,
        json!({
            "model": "qwen3-32b-awq",
            "instructions": "Be brief.",
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "What is the answer?" }] }],
            "stream": true,
            "max_output_tokens": 2048,
            "user": "u".repeat(64),
        })
    );
}

#[test]
fn request_body_merges_extra_body_without_controlled_keys() {
    let extra: Map<String, Value> = json!({
        "chat_template_kwargs": { "enable_thinking": true },
        "model": "hijacked",
        "tools": [{ "type": "function", "name": "hijacked" }],
        "metadata": { "hijacked": "yes" },
    })
    .as_object()
    .unwrap()
    .clone();
    let params = ModelApiParams {
        top_p: Some(0.9),
        extra_body: Some(extra),
        ..ModelApiParams::default()
    };
    let body = build_request_body(&request(vec![], params), false);
    assert_eq!(
        body["chat_template_kwargs"],
        json!({ "enable_thinking": true })
    );
    assert_eq!(body["model"], json!("qwen3-32b-awq"));
    assert!(body.get("tools").is_none());
    assert!(body.get("metadata").is_none());
    assert_eq!(body["top_p"], json!(0.9));
    assert_eq!(body["stream"], json!(false));
}

// ── <think> splitting ────────────────────────────────────────────────────

fn split(deltas: &[&str]) -> Vec<ThinkSegment> {
    let mut splitter = ThinkSplitter::default();
    let mut out: Vec<ThinkSegment> = deltas
        .iter()
        .flat_map(|delta| splitter.push(delta))
        .collect();
    out.extend(splitter.finish());
    out
}

fn joined(segments: &[ThinkSegment]) -> (String, String) {
    let mut text = String::new();
    let mut reasoning = String::new();
    for segment in segments {
        match segment {
            ThinkSegment::Text(t) => text.push_str(t),
            ThinkSegment::Reasoning(r) => reasoning.push_str(r),
        }
    }
    (text, reasoning)
}

#[test]
fn think_block_is_reasoning_and_the_rest_is_text() {
    let segments = split(&[
        "<think>Let me ",
        "think about it.</think>",
        "The answer ",
        "is 42.",
    ]);
    assert_eq!(
        segments,
        vec![
            ThinkSegment::Reasoning("Let me ".to_owned()),
            ThinkSegment::Reasoning("think about it.".to_owned()),
            ThinkSegment::Text("The answer ".to_owned()),
            ThinkSegment::Text("is 42.".to_owned()),
        ]
    );
}

#[test]
fn tags_split_across_deltas_are_recognized() {
    let segments = split(&["<th", "ink>dee", "p</thi", "nk>\n\nAns", "wer <b>bold</b>"]);
    let (text, reasoning) = joined(&segments);
    assert_eq!(reasoning, "deep");
    assert_eq!(text, "Answer <b>bold</b>");
    for segment in &segments {
        let (ThinkSegment::Text(s) | ThinkSegment::Reasoning(s)) = segment;
        assert!(
            !s.contains("<think") && !s.contains("</think"),
            "{segment:?}"
        );
    }
}

#[test]
fn every_split_point_gives_the_same_result() {
    let full = "<think>a < b</think>\nResult: x <th y";
    for cut in 1..full.len() {
        let (head, tail) = full.split_at(cut);
        assert_eq!(
            joined(&split(&[head, tail])),
            ("Result: x <th y".to_owned(), "a < b".to_owned()),
            "cut at {cut}"
        );
    }
}

#[test]
fn text_without_think_and_unclosed_think() {
    assert_eq!(joined(&split(&["plain ", "answer"])).0, "plain answer");
    let (text, reasoning) = joined(&split(&["<think>never closed"]));
    assert_eq!((text.as_str(), reasoning.as_str()), ("", "never closed"));
    assert_eq!(strip_think("<think>r</think>\n\nanswer"), "answer");
}

// ── Stream translation ───────────────────────────────────────────────────

fn sse(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn completed_wire() -> String {
    let text = "<think>Let me think about it.</think>The answer is 42.";
    [
        sse("response.created", &json!({ "response": { "id": "resp_v" } })),
        sse(
            "response.output_text.delta",
            &json!({ "item_id": "m", "content_index": 0, "delta": "<think>Let me " }),
        ),
        sse(
            "response.output_text.delta",
            &json!({ "item_id": "m", "content_index": 0, "delta": "think about it.</think>" }),
        ),
        sse(
            "response.output_text.delta",
            &json!({ "item_id": "m", "content_index": 0, "delta": "The answer is 42." }),
        ),
        sse(
            "response.completed",
            &json!({ "response": {
                "id": "resp_v", "status": "completed",
                "output": [{ "type": "message", "content": [{ "type": "output_text", "text": text, "annotations": [] }] }],
                "usage": { "input_tokens": 12, "output_tokens": 8 },
            }}),
        ),
    ]
    .concat()
}

#[test]
fn stream_translation_splits_reasoning_and_strips_the_output_text() {
    let mut parser = SseParser::default();
    let mut translator = ResponsesTranslator::with_think_split();
    let mut out = Vec::new();
    for event in parser.push(completed_wire().as_bytes()) {
        translator.translate(event, &mut out);
    }
    assert_eq!(
        out,
        vec![
            LlmEvent::ReasoningDelta("Let me ".to_owned()),
            LlmEvent::ReasoningDelta("think about it.".to_owned()),
            LlmEvent::TextDelta("The answer is 42.".to_owned()),
            LlmEvent::Completed {
                response_id: Some("resp_v".to_owned()),
                usage: Some(LlmUsage {
                    input_tokens: 12,
                    output_tokens: 8,
                    ..LlmUsage::default()
                }),
                incomplete_reason: None,
                output_text: "The answer is 42.".to_owned(),
            },
        ]
    );
}

#[test]
fn reasoning_text_events_are_reasoning_only_for_vllm() {
    let wire = sse(
        "response.reasoning_text.delta",
        &json!({ "item_id": "r", "delta": "hmm" }),
    );
    let mut parser = SseParser::default();
    let mut vllm = ResponsesTranslator::with_think_split();
    let mut openai = ResponsesTranslator::default();
    let (mut vllm_out, mut openai_out) = (Vec::new(), Vec::new());
    for event in parser.push(wire.as_bytes()) {
        vllm.translate(event.clone(), &mut vllm_out);
        openai.translate(event, &mut openai_out);
    }
    assert_eq!(vllm_out, vec![LlmEvent::ReasoningDelta("hmm".to_owned())]);
    assert!(openai_out.is_empty());
}

// ── Adapter over a fake gateway ──────────────────────────────────────────

fn target() -> ProviderTarget {
    ProviderTarget {
        provider_id: "vllm".to_owned(),
        kind: ProviderKind::VllmResponses,
        alias: "mock-vllm".to_owned(),
        api_path: "/vllm/v1/responses".to_owned(),
        tenant_id: Uuid::from_u128(7),
        tenant_override: false,
    }
}

fn adapter(gateway: &Arc<FakeGateway>) -> VllmResponsesAdapter {
    VllmResponsesAdapter::new(gateway.clone(), Arc::new(StaticSystemContext::default()))
}

#[tokio::test]
async fn stream_sends_the_reduced_body_through_oagw() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(sse_response(vec![completed_wire()])));
    let events: Vec<LlmEvent> = adapter(&gateway)
        .stream(
            &target(),
            request(all_tools(), ModelApiParams::default()),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .collect()
        .await;
    assert!(events.contains(&LlmEvent::ReasoningDelta("Let me ".to_owned())));
    let sent = lock(&gateway.proxied)[0].clone();
    assert_eq!(sent.uri, "/mock-vllm/vllm/v1/responses");
    assert!(sent.body.get("tools").is_none());
    assert!(sent.body.get("metadata").is_none());
}

#[tokio::test]
async fn complete_strips_the_think_block() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        200,
        &json!({ "id": "resp_s", "status": "completed",
                 "output": [{ "type": "message", "content": [{ "type": "output_text",
                    "text": "<think>planning</think>\nsummary" }] }] }),
        &[],
        None,
    )));
    let completion = adapter(&gateway)
        .complete(&target(), request(vec![], ModelApiParams::default()))
        .await
        .unwrap();
    assert_eq!(completion.text, "summary");
}
