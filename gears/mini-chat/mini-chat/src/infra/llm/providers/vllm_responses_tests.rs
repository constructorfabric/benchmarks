#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use mini_chat_sdk::{ModelApiParams, UsageTokens};
use oagw_sdk::api::ErrorSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::ProviderKind;
use crate::infra::llm::fake_gw::{FakeGw, Reply, client, http_error, json_ok, sse};
use crate::infra::llm::types::{
    ContentPart, InputItem, LlmClient, LlmEvent, LlmRequest, ProviderError, RequestMetadata,
    ResolvedProvider, ToolSpec, provider_user,
};

const TENANT: &str = "6f1d7a52-0f6e-4c37-9a3c-0d7a8f3c2b11";
const USER: &str = "0b9e4c1d-2f3a-4b5c-8d6e-7f8091a2b3c4";

fn provider() -> ResolvedProvider {
    ResolvedProvider {
        provider_id: "vllm".to_owned(),
        kind: ProviderKind::VllmResponses,
        alias: "vllm.internal".to_owned(),
        api_path: "/v1/responses".to_owned(),
        storage: None,
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "qwen3".to_owned(),
        instructions: "be helpful".to_owned(),
        input: vec![InputItem::Message {
            role: "user",
            content: vec![ContentPart::InputText("hi".to_owned())],
        }],
        tools: vec![],
        max_output_tokens: 512,
        api_params: ModelApiParams::default(),
        max_tool_calls: Some(2),
        user: provider_user(TENANT, USER),
        metadata: RequestMetadata {
            tenant_id: TENANT.to_owned(),
            user_id: USER.to_owned(),
            chat_id: "c0ffee00-0000-4000-8000-000000000001".to_owned(),
            request_type: "chat",
            feature: "web_search".to_owned(),
        },
        stream: false,
    }
}

async fn run(reply: Reply) -> Vec<LlmEvent> {
    let gw = FakeGw::with(reply);
    let (client, _) = client(&gw);
    let stream = client
        .stream(&provider(), request(), CancellationToken::new())
        .await
        .unwrap();
    stream.collect().await
}

fn delta(text: &str) -> (&'static str, Value) {
    (
        "response.output_text.delta",
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": text}),
    )
}

fn completed() -> (&'static str, Value) {
    (
        "response.completed",
        json!({"type": "response.completed", "response": {
            "id": "resp_v1", "status": "completed", "output": [],
            "usage": {"input_tokens": 10, "output_tokens": 4}
        }}),
    )
}

fn usage() -> UsageTokens {
    UsageTokens {
        input_tokens: 10,
        output_tokens: 4,
        ..UsageTokens::default()
    }
}

#[tokio::test]
async fn request_drops_all_tools_and_metadata() {
    let gw = FakeGw::with(sse(&[completed()]));
    let (client, _) = client(&gw);
    let mut req = request();
    req.tools = vec![
        ToolSpec::WebSearch {
            search_context_size: "low".to_owned(),
        },
        ToolSpec::CodeInterpreter {
            file_ids: vec!["file-x".to_owned()],
        },
        ToolSpec::Function {
            name: "search_knowledge".to_owned(),
            description: "kb".to_owned(),
            parameters: json!({"type": "object"}),
        },
    ];
    req.api_params.extra_body = Some(
        json!({"top_k": 20, "metadata": {"x": 1}})
            .as_object()
            .unwrap()
            .clone(),
    );
    let stream = client
        .stream(&provider(), req, CancellationToken::new())
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;
    let cap = gw.last();
    assert_eq!(cap.uri, "/vllm.internal/v1/responses");
    let body = cap.body;
    for key in ["tools", "include", "max_tool_calls", "metadata"] {
        assert!(body.get(key).is_none(), "{key} must not be sent");
    }
    assert_eq!(body["model"], "qwen3");
    assert_eq!(body["instructions"], "be helpful");
    assert_eq!(body["max_output_tokens"], 512);
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["user"],
        "6f1d7a520f6e4c379a3c0d7a8f3c2b110b9e4c1d2f3a4b5c8d6e7f8091a2b3c4"
    );
    assert_eq!(
        body["input"],
        json!([{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
    );
    assert_eq!(body["top_k"], 20);
}

#[tokio::test]
async fn think_blocks_become_reasoning_deltas() {
    let events = run(sse(&[
        delta("<thi"),
        delta("nk>ponder"),
        delta("ing</th"),
        delta("ink>Ans"),
        delta("wer <"),
        delta("b>!"),
        completed(),
    ]))
    .await;
    assert_eq!(
        events,
        vec![
            LlmEvent::ReasoningDelta("ponder".to_owned()),
            LlmEvent::ReasoningDelta("ing".to_owned()),
            LlmEvent::TextDelta("Ans".to_owned()),
            LlmEvent::TextDelta("wer ".to_owned()),
            LlmEvent::TextDelta("<b>!".to_owned()),
            LlmEvent::Completed {
                usage: Some(usage()),
                response_id: Some("resp_v1".to_owned()),
                incomplete_reason: None,
            },
        ]
    );
}

#[tokio::test]
async fn held_back_partial_tag_is_flushed_before_completion() {
    let events = run(sse(&[delta("a <"), completed()])).await;
    assert_eq!(
        events[..2],
        [
            LlmEvent::TextDelta("a ".to_owned()),
            LlmEvent::TextDelta("<".to_owned()),
        ]
    );
}

#[tokio::test]
async fn reasoning_text_events_are_reasoning_deltas() {
    let events = run(sse(&[
        (
            "response.reasoning_text.delta",
            json!({"type": "response.reasoning_text.delta", "delta": "hmm"}),
        ),
        delta("Hi"),
        completed(),
    ]))
    .await;
    assert_eq!(events[0], LlmEvent::ReasoningDelta("hmm".to_owned()));
    assert_eq!(events[1], LlmEvent::TextDelta("Hi".to_owned()));
}

#[tokio::test]
async fn response_failed_maps_to_provider_error() {
    let events = run(sse(&[(
        "response.failed",
        json!({"type": "response.failed", "response": {"error": {"code": "server_error", "message": "engine died"}}}),
    )]))
    .await;
    assert_eq!(
        events,
        vec![LlmEvent::Failed {
            error: ProviderError::provider("engine died"),
            usage: None,
        }]
    );
}

#[tokio::test]
async fn http_429_is_rate_limited() {
    let gw = FakeGw::with(http_error(
        429,
        ErrorSource::Upstream,
        vec![("retry-after", "3".to_owned())],
        &json!({}),
    ));
    let (client, _) = client(&gw);
    let Err(err) = client
        .stream(&provider(), request(), CancellationToken::new())
        .await
    else {
        panic!("expected an error");
    };
    assert_eq!(err.code, "rate_limited");
    assert_eq!(err.retry_after_secs, Some(3));
}

#[tokio::test]
async fn complete_strips_think_blocks() {
    let gw = FakeGw::with(json_ok(&json!({
        "id": "resp_v2", "status": "completed",
        "usage": {"input_tokens": 10, "output_tokens": 4},
        "output": [{"type": "message", "content": [
            {"type": "output_text", "text": "<think>plan</think>Summary"}
        ]}]
    })));
    let (client, _) = client(&gw);
    let out = client.complete(&provider(), request()).await.unwrap();
    assert_eq!(out.text, "Summary");
    assert_eq!(out.usage, Some(usage()));
    assert!(gw.last().body.get("metadata").is_none());
}
