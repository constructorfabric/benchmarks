#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use mini_chat_sdk::{ModelApiParams, UsageTokens};
use oagw_sdk::api::ErrorSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::fake_gateway::{FakeBody, FakeGateway, ProxyScript, errs};
use super::translate::{CODE_OUTPUT_MAX_CHARS, ThinkSplitter, strip_think};
use super::*;
use crate::config::{MiniChatConfig, ProviderEntry};
use crate::infra::llm::{
    ContentPart, FileStorage, InputMessage, InputRole, RawCitation, RequestType, StorageError,
    ToolSpec, VectorFileStatus,
};
use crate::infra::s2s::S2sContextProvider;

const TENANT: Uuid = Uuid::from_u128(0xaaaa_0000_0000_0000_0000_0000_0000_0001);
const OTHER_TENANT: Uuid = Uuid::from_u128(0xaaaa_0000_0000_0000_0000_0000_0000_0002);

fn entry(v: Value) -> ProviderEntry {
    serde_json::from_value(v).unwrap()
}

fn config() -> MiniChatConfig {
    let mut cfg = MiniChatConfig::default();
    cfg.providers.insert(
        "azure".into(),
        entry(json!({
            "kind": "openai_responses",
            "host": "res.openai.azure.com",
            "api_path": "/openai/v1/responses?api-version=preview",
            "storage_kind": "azure",
            "api_version": "2025-04-01-preview",
            "tenant_overrides": {
                OTHER_TENANT.to_string(): { "host": "other.openai.azure.com" }
            }
        })),
    );
    cfg.providers.insert(
        "deploy".into(),
        entry(json!({
            "kind": "openai_responses",
            "host": "dep.openai.azure.com",
            "api_path": "/openai/deployments/{model}/responses?api-version=2025-01-01",
            "storage_kind": "azure",
            "api_version": "2025-01-01",
        })),
    );
    cfg.providers.insert(
        "vllm".into(),
        entry(json!({ "kind": "vllm_responses", "host": "vllm.local", "storage_kind": "openai" })),
    );
    cfg.providers.insert(
        "chat".into(),
        entry(json!({
            "kind": "openai_chat_completions",
            "host": "chat.local",
            "api_path": "/v1/chat/completions",
            "storage_kind": "openai"
        })),
    );
    cfg.providers.insert(
        "anthropic".into(),
        entry(json!({
            "kind": "anthropic_messages",
            "host": "api.anthropic.com",
            "api_path": "/v1/messages",
            "storage_kind": "openai",
            "rag_provider": "openai"
        })),
    );
    cfg.fill_aliases();
    cfg
}

fn s2s() -> Arc<S2sContextProvider> {
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::from_u128(7))
        .subject_tenant_id(TENANT)
        .build()
        .unwrap();
    Arc::new(S2sContextProvider::fixed(ctx))
}

fn client(gw: &Arc<FakeGateway>) -> OagwProviderClient {
    let cfg = config();
    OagwProviderClient::new(
        Arc::new(ProviderResolver::new(&cfg)),
        Arc::clone(gw) as Arc<dyn ServiceGatewayClientV1>,
        s2s(),
    )
}

fn req(provider: &str) -> LlmRequest {
    let mut metadata = BTreeMap::new();
    metadata.insert("tenant_id".to_owned(), TENANT.to_string());
    metadata.insert("request_type".to_owned(), "chat".to_owned());
    metadata.insert("feature".to_owned(), "none".to_owned());
    LlmRequest {
        provider_id: provider.to_owned(),
        tenant_id: TENANT,
        model: "gpt-4.1".to_owned(),
        instructions: "be helpful".to_owned(),
        input: vec![InputMessage::text(InputRole::User, "hi")],
        tool_exchanges: vec![],
        tools: vec![],
        max_output_tokens: 1000,
        max_tool_calls: Some(2),
        api_params: ModelApiParams::default(),
        user: "ab".repeat(32),
        metadata,
        request_type: RequestType::Chat,
        stream: true,
    }
}

fn ev(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn completed(text: &str, usage: &Value) -> String {
    ev(
        "response.completed",
        &json!({
            "type": "response.completed",
            "response": {
                "id": "resp_abc123",
                "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": text, "annotations": []}]}],
                "usage": usage,
            }
        }),
    )
}

async fn run(gw: &Arc<FakeGateway>, r: LlmRequest) -> Vec<LlmEvent> {
    client(gw)
        .stream(r, CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await
}

fn texts(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn last_completion(events: &[LlmEvent]) -> LlmCompletion {
    match events.last().unwrap() {
        LlmEvent::Completed(c) => c.clone(),
        other => panic!("expected Completed, got {other:?}"),
    }
}

fn last_failure(events: &[LlmEvent]) -> LlmFailure {
    match events.last().unwrap() {
        LlmEvent::Failed(f) => f.clone(),
        other => panic!("expected Failed, got {other:?}"),
    }
}

async fn stream_err(gw: &Arc<FakeGateway>, script: ProxyScript) -> LlmFailure {
    gw.push(script);
    match client(gw)
        .stream(req("openai"), CancellationToken::new())
        .await
    {
        Err(f) => f,
        Ok(_) => panic!("expected pre-stream failure"),
    }
}

// ── request shape ──────────────────────────────────────────────────────────

#[tokio::test]
async fn openai_basic_request_shape() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    run(&gw, req("openai")).await;

    let (method, uri, headers, _) = gw.proxy_calls().pop().unwrap();
    assert_eq!(method, http::Method::POST);
    assert_eq!(uri, "/api.openai.com/v1/responses");
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
    let body = gw.last_json_body();
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], "be helpful");
    assert_eq!(body["max_output_tokens"], 1000);
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
    assert_eq!(body["metadata"]["request_type"], "chat");
    assert_eq!(body["metadata"]["feature"], "none");
    assert_eq!(
        body["input"],
        json!([{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
    );
    for absent in [
        "tools",
        "include",
        "max_tool_calls",
        "temperature",
        "top_p",
        "reasoning",
    ] {
        assert!(body.get(absent).is_none(), "{absent} must be absent");
    }
}

#[tokio::test]
async fn empty_instructions_are_omitted() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    let mut r = req("openai");
    r.instructions = String::new();
    run(&gw, r).await;
    assert!(gw.last_json_body().get("instructions").is_none());
}

#[tokio::test]
async fn openai_request_with_all_tools() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    let mut r = req("openai");
    r.tools = vec![
        ToolSpec::FileSearch {
            vector_store_ids: vec!["vs_1".into()],
            max_num_results: 5,
        },
        ToolSpec::WebSearch {
            search_context_size: "low".into(),
        },
        ToolSpec::CodeInterpreter {
            file_ids: vec!["file-1".into(), "file-2".into()],
        },
    ];
    r.max_tool_calls = Some(3);
    run(&gw, r).await;
    let body = gw.last_json_body();
    assert_eq!(
        body["tools"],
        json!([
            {"type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 5},
            {"type": "web_search", "search_context_size": "low"},
            {"type": "code_interpreter", "container": {"type": "auto", "file_ids": ["file-1", "file-2"]}}
        ])
    );
    assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(body["max_tool_calls"], 3);
}

#[tokio::test]
async fn openai_request_file_search_only_has_no_include() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    let mut r = req("openai");
    r.tools = vec![ToolSpec::FileSearch {
        vector_store_ids: vec!["vs_1".into()],
        max_num_results: 4,
    }];
    r.max_tool_calls = None;
    run(&gw, r).await;
    let body = gw.last_json_body();
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert!(body.get("include").is_none());
    assert!(body.get("max_tool_calls").is_none());
}

#[tokio::test]
async fn multimodal_and_assistant_history() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    let mut r = req("openai");
    r.input = vec![
        InputMessage::text(InputRole::System, "summary"),
        InputMessage::text(InputRole::Assistant, "earlier answer"),
        InputMessage {
            role: InputRole::User,
            content: vec![
                ContentPart::Text("what is this?".into()),
                ContentPart::Image {
                    file_id: "file-img1".into(),
                },
                ContentPart::Image {
                    file_id: "file-img2".into(),
                },
            ],
        },
    ];
    run(&gw, r).await;
    assert_eq!(
        gw.last_json_body()["input"],
        json!([
            {"role": "system", "content": [{"type": "input_text", "text": "summary"}]},
            {"role": "assistant", "content": [{"type": "output_text", "text": "earlier answer"}]},
            {"role": "user", "content": [
                {"type": "input_text", "text": "what is this?"},
                {"type": "input_image", "file_id": "file-img1"},
                {"type": "input_image", "file_id": "file-img2"}
            ]}
        ])
    );
}

#[tokio::test]
async fn api_params_and_extra_body() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    let mut r = req("openai");
    let mut extra = serde_json::Map::new();
    extra.insert("service_tier".into(), json!("flex"));
    extra.insert("model".into(), json!("evil"));
    extra.insert("stream".into(), json!(false));
    extra.insert("metadata".into(), json!({}));
    r.api_params = ModelApiParams {
        temperature: Some(0.5),
        presence_penalty: Some(0.25),
        reasoning_effort: Some("low".into()),
        extra_body: Some(extra),
        ..ModelApiParams::default()
    };
    run(&gw, r).await;
    let body = gw.last_json_body();
    assert_eq!(body["temperature"], 0.5);
    assert_eq!(body["presence_penalty"], 0.25);
    assert!(body.get("top_p").is_none());
    assert!(body.get("frequency_penalty").is_none());
    assert_eq!(body["reasoning"], json!({"effort": "low"}));
    assert_eq!(body["service_tier"], "flex");
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["stream"], true);
    assert_eq!(body["metadata"]["feature"], "none");
}

#[tokio::test]
async fn vllm_request_drops_tools_and_metadata() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    let mut r = req("vllm");
    r.tools = vec![
        ToolSpec::WebSearch {
            search_context_size: "low".into(),
        },
        ToolSpec::CodeInterpreter { file_ids: vec![] },
    ];
    run(&gw, r).await;
    let (_, uri, _, _) = gw.proxy_calls().pop().unwrap();
    assert_eq!(uri, "/vllm.local/v1/responses");
    let body = gw.last_json_body();
    for absent in ["tools", "include", "max_tool_calls", "metadata"] {
        assert!(body.get(absent).is_none(), "{absent} must be absent");
    }
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn model_placeholder_and_query_in_api_path() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    run(&gw, req("deploy")).await;
    let (_, uri, _, _) = gw.proxy_calls().pop().unwrap();
    assert_eq!(
        uri,
        "/dep.openai.azure.com/openai/deployments/gpt-4.1/responses?api-version=2025-01-01"
    );
}

#[tokio::test]
async fn tenant_override_alias_is_used() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    let mut r = req("azure");
    r.tenant_id = OTHER_TENANT;
    run(&gw, r).await;
    let (_, uri, _, _) = gw.proxy_calls().pop().unwrap();
    assert_eq!(
        uri,
        "/other.openai.azure.com/openai/v1/responses?api-version=preview"
    );
}

#[tokio::test]
async fn chat_completions_request_shape() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec!["data: [DONE]\n\n".into()]));
    let mut r = req("chat");
    r.tools = vec![ToolSpec::WebSearch {
        search_context_size: "low".into(),
    }];
    r.api_params.stop = vec!["END".into()];
    r.api_params.top_p = Some(0.9);
    run(&gw, r).await;
    let (_, uri, _, _) = gw.proxy_calls().pop().unwrap();
    assert_eq!(uri, "/chat.local/v1/chat/completions");
    let body = gw.last_json_body();
    assert_eq!(
        body["messages"],
        json!([{"role": "system", "content": "be helpful"}, {"role": "user", "content": "hi"}])
    );
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["max_completion_tokens"], 1000);
    assert_eq!(body["stop"], json!(["END"]));
    assert_eq!(body["top_p"], 0.9);
    assert!(body.get("tools").is_none());
    assert!(body.get("metadata").is_none());
}

#[tokio::test]
async fn anthropic_request_shape() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![ev(
        "message_stop",
        &json!({"type": "message_stop"}),
    )]));
    let mut r = req("anthropic");
    r.tools = vec![
        ToolSpec::FileSearch {
            vector_store_ids: vec!["vs_1".into()],
            max_num_results: 3,
        },
        ToolSpec::WebSearch {
            search_context_size: "low".into(),
        },
    ];
    run(&gw, r).await;
    let (_, uri, headers, _) = gw.proxy_calls().pop().unwrap();
    assert_eq!(uri, "/api.anthropic.com/v1/messages");
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    let body = gw.last_json_body();
    assert_eq!(body["system"], "be helpful");
    assert_eq!(body["max_tokens"], 1000);
    assert_eq!(body["metadata"]["user_id"].as_str().unwrap().len(), 64);
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
    );
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "web_search");
}

// ── SSE translation (Responses) ────────────────────────────────────────────

#[tokio::test]
async fn translates_every_responses_event() {
    let gw = FakeGateway::new();
    let usage = json!({
        "input_tokens": 120,
        "output_tokens": 30,
        "input_tokens_details": {"cached_tokens": 20},
        "output_tokens_details": {"reasoning_tokens": 5}
    });
    gw.push(ProxyScript::sse(vec![
        ev(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_1"}}),
        ),
        ev(
            "response.file_search_call.searching",
            &json!({"type": "response.file_search_call.searching"}),
        ),
        ev(
            "response.file_search_call.completed",
            &json!({"type": "response.file_search_call.completed", "results": [{}, {}]}),
        ),
        ev("response.web_search_call.searching", &json!({})),
        ev("response.web_search_call.completed", &json!({})),
        ev("response.code_interpreter_call.in_progress", &json!({})),
        ev("response.code_interpreter_call.interpreting", &json!({})),
        ev("response.code_interpreter_call.completed", &json!({})),
        ev(
            "response.output_item.done",
            &json!({"item": {"type": "code_interpreter_call", "outputs": [
                {"type": "logs", "logs": "line1"},
                {"type": "image", "url": "x"},
                {"type": "logs", "logs": "line2"}
            ]}}),
        ),
        ev(
            "response.output_item.done",
            &json!({"item": {"type": "message"}}),
        ),
        // chunked across two transport chunks
        "event: response.output_text.delta\ndata: {\"delta\":\"Hel".to_owned(),
        "lo\"}\n\n".to_owned(),
        ev("response.output_text.delta", &json!({"delta": " world"})),
        completed("Hello world", &usage),
        ev(
            "response.output_text.delta",
            &json!({"delta": "after terminal"}),
        ),
    ]));
    let events = run(&gw, req("openai")).await;

    let tools: Vec<(bool, String, Value)> = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolStart { name, details } => Some((true, name.clone(), details.clone())),
            LlmEvent::ToolDone { name, details } => Some((false, name.clone(), details.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        tools,
        vec![
            (true, "file_search".into(), json!({})),
            (false, "file_search".into(), json!({"files_searched": 2})),
            (true, "web_search".into(), json!({})),
            (false, "web_search".into(), json!({})),
            (true, "code_interpreter".into(), json!({})),
            (
                false,
                "code_interpreter".into(),
                json!({"output": "line1\nline2"})
            ),
        ]
    );
    assert_eq!(texts(&events), "Hello world");
    let c = last_completion(&events);
    assert_eq!(c.response_id.as_deref(), Some("resp_abc123"));
    assert_eq!(c.output_text, "Hello world");
    assert_eq!(c.incomplete_reason, None);
    assert_eq!(
        c.usage,
        Some(UsageTokens {
            input_tokens: 120,
            output_tokens: 30,
            cache_read_input_tokens: 20,
            cache_write_input_tokens: 0,
            reasoning_tokens: 5,
        })
    );
}

#[tokio::test]
async fn event_name_falls_back_to_type_field() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![
        format!(
            "data: {}\n\n",
            json!({"type": "response.output_text.delta", "delta": "a"})
        ),
        ev(
            "message",
            &json!({"type": "response.output_text.delta", "delta": "b"}),
        ),
        format!(
            "data: {}\n\n",
            json!({"type": "response.completed", "response": {"output": []}})
        ),
    ]));
    let events = run(&gw, req("openai")).await;
    assert_eq!(texts(&events), "ab");
    let c = last_completion(&events);
    assert!(c.usage.is_none(), "absent usage object maps to None");
    assert!(c.response_id.is_none());
}

#[tokio::test]
async fn incomplete_maps_to_completed_with_reason() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![
        ev("response.output_text.delta", &json!({"delta": "par"})),
        ev(
            "response.incomplete",
            &json!({"response": {"id": "resp_x", "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": {"input_tokens": 1, "output_tokens": 2}, "output": []}}),
        ),
    ]));
    let c = last_completion(&run(&gw, req("openai")).await);
    assert_eq!(c.incomplete_reason.as_deref(), Some("max_output_tokens"));
    assert_eq!(c.usage.unwrap().output_tokens, 2);
}

#[tokio::test]
async fn response_failed_is_sanitized_provider_error_with_usage() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![ev(
        "response.failed",
        &json!({"response": {
            "id": "resp_failed1",
            "error": {"code": "server_error", "message": "bad file file-abcdefghijklmnop at https://x.y/z"},
            "usage": {"input_tokens": 10, "output_tokens": 0}
        }}),
    )]));
    let f = last_failure(&run(&gw, req("openai")).await);
    assert_eq!(f.code, "provider_error");
    assert_eq!(f.message, "bad file [provider_id] at [url]");
    assert_eq!(f.usage.unwrap().input_tokens, 10);
    assert_eq!(f.response_id.as_deref(), Some("resp_failed1"));
    assert!(!f.context_length_exceeded);
}

#[tokio::test]
async fn response_failed_falls_back_to_top_level_error_and_flags_context_length() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![ev(
        "response.failed",
        &json!({"response": {"id": "r"}, "error": {"code": "context_length_exceeded", "message": "too long"}}),
    )]));
    let f = last_failure(&run(&gw, req("openai")).await);
    assert_eq!(f.message, "too long");
    assert!(f.context_length_exceeded);
    assert!(f.usage.is_none());
}

#[tokio::test]
async fn sse_error_event_variants() {
    for (data, expected) in [
        (
            json!({"type": "error", "code": "rate", "message": "slow down sk-abcdefghijklmnop"})
                .to_string(),
            "slow down [credential]",
        ),
        (
            json!({"error": {"message": "nested message"}}).to_string(),
            "nested message",
        ),
        (
            json!({"response": {"error": {"message": "failed-like"}}}).to_string(),
            "failed-like",
        ),
        ("oops not json".to_owned(), "oops not json"),
    ] {
        let gw = FakeGateway::new();
        gw.push(ProxyScript::sse(vec![
            ev("response.output_text.delta", &json!({"delta": "x"})),
            format!("event: error\ndata: {data}\n\n"),
        ]));
        let events = run(&gw, req("openai")).await;
        let f = last_failure(&events);
        assert_eq!(f.code, "provider_error");
        assert_eq!(f.message, expected);
    }
}

#[tokio::test]
async fn citations_web_and_file() {
    let gw = FakeGateway::new();
    let text = "Héllo wörld, see source.";
    gw.push(ProxyScript::sse(vec![ev(
        "response.completed",
        &json!({"response": {"id": "resp_c", "output": [
            {"type": "web_search_call"},
            {"type": "message", "content": [{"type": "output_text", "text": text, "annotations": [
                {"type": "url_citation", "url": "https://a.example", "title": "A", "start_index": 0, "end_index": 5},
                {"type": "url_citation", "url": "https://b.example", "title": "B", "start_index": 2, "end_index": 99},
                {"type": "url_citation", "url": "https://c.example", "title": "C", "text": "own snippet", "start_index": 1, "end_index": 3},
                {"type": "url_citation", "url": "https://d.example"},
                {"type": "file_citation", "file_id": "file-1", "filename": "a.pdf", "index": 3},
                {"type": "file_citation", "file_id": "file-2", "start_index": 4, "end_index": 8},
                {"type": "container_file_citation", "file_id": "cfile"}
            ]}]}
        ]}}),
    )]));
    let c = last_completion(&run(&gw, req("openai")).await);
    assert_eq!(
        c.citations,
        vec![
            RawCitation::Web {
                url: "https://a.example".into(),
                title: "A".into(),
                snippet: "Héllo".into(),
                span: Some((0, 5)),
            },
            RawCitation::Web {
                url: "https://b.example".into(),
                title: "B".into(),
                snippet: String::new(),
                span: Some((2, 99)),
            },
            RawCitation::Web {
                url: "https://c.example".into(),
                title: "C".into(),
                snippet: "own snippet".into(),
                span: Some((1, 3)),
            },
            RawCitation::Web {
                url: "https://d.example".into(),
                title: String::new(),
                snippet: String::new(),
                span: None,
            },
            RawCitation::File {
                file_id: "file-1".into(),
                filename: Some("a.pdf".into()),
                span: None,
            },
            RawCitation::File {
                file_id: "file-2".into(),
                filename: None,
                span: Some((4, 8)),
            },
        ]
    );
    assert_eq!(c.output_text, text);
}

#[test]
fn code_interpreter_output_is_capped() {
    let long = "é".repeat(CODE_OUTPUT_MAX_CHARS + 10);
    let out =
        translate::code_interpreter_output(&json!({"outputs": [{"type": "logs", "logs": long}]}));
    assert!(out.ends_with("...[truncated]"));
    assert_eq!(
        out.chars().count(),
        CODE_OUTPUT_MAX_CHARS + "...[truncated]".len()
    );
    let exact = "a".repeat(CODE_OUTPUT_MAX_CHARS);
    assert_eq!(
        translate::code_interpreter_output(&json!({"outputs": [{"type": "logs", "logs": exact}]})),
        exact
    );
}

#[tokio::test]
async fn transport_end_without_terminal_is_provider_error() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![ev(
        "response.output_text.delta",
        &json!({"delta": "x"}),
    )]));
    let events = run(&gw, req("openai")).await;
    assert_eq!(events.len(), 2);
    let f = last_failure(&events);
    assert_eq!(f.code, "provider_error");
}

#[tokio::test]
async fn sse_without_content_type_is_still_parsed() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::Response {
        status: 200,
        headers: vec![],
        body: FakeBody::Chunks(vec![
            ev("response.output_text.delta", &json!({"delta": "x"})),
            completed("x", &json!({})),
        ]),
        source: None,
    });
    let events = run(&gw, req("openai")).await;
    assert_eq!(texts(&events), "x");
    last_completion(&events);
}

#[tokio::test]
async fn json_body_instead_of_stream() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::json(
        200,
        &json!({"id": "resp_j", "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "whole"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 4}}),
    ));
    let events = run(&gw, req("openai")).await;
    assert_eq!(texts(&events), "whole");
    assert_eq!(last_completion(&events).usage.unwrap().input_tokens, 3);
}

#[tokio::test]
async fn cancellation_drops_upstream_without_terminal() {
    let gw = FakeGateway::new();
    let dropped = Arc::new(AtomicBool::new(false));
    gw.push(ProxyScript::Response {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: FakeBody::Hang(
            vec![ev("response.output_text.delta", &json!({"delta": "first"}))],
            Arc::clone(&dropped),
        ),
        source: None,
    });
    let cancel = CancellationToken::new();
    let mut stream = client(&gw)
        .stream(req("openai"), cancel.clone())
        .await
        .unwrap();
    match stream.next().await.unwrap() {
        LlmEvent::TextDelta(t) => assert_eq!(t, "first"),
        other => panic!("unexpected {other:?}"),
    }
    assert!(!dropped.load(Ordering::SeqCst));
    cancel.cancel();
    let rest = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("stream must end after cancel");
    assert!(rest.is_none(), "no terminal event after cancel");
    assert!(
        dropped.load(Ordering::SeqCst),
        "upstream body dropped on cancel"
    );
}

#[tokio::test]
async fn cancel_before_response_yields_empty_stream() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("x", &json!({}))]));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let events: Vec<LlmEvent> = client(&gw)
        .stream(req("openai"), cancel)
        .await
        .unwrap()
        .collect()
        .await;
    assert!(events.is_empty());
}

// ── vLLM <think> ───────────────────────────────────────────────────────────

#[test]
fn think_splitter_handles_split_tags() {
    let mut s = ThinkSplitter::default();
    let mut out = Vec::new();
    for d in ["Intro <thi", "nk>reas", "oning</th", "ink>answer <", "b>"] {
        out.extend(s.push(d));
    }
    out.extend(s.flush());
    let mut text = String::new();
    let mut reasoning = String::new();
    for e in out {
        match e {
            LlmEvent::TextDelta(t) => text.push_str(&t),
            LlmEvent::ReasoningDelta(t) => reasoning.push_str(&t),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(text, "Intro answer <b>");
    assert_eq!(reasoning, "reasoning");
}

#[test]
fn strip_think_variants() {
    assert_eq!(strip_think("<think>x</think>\nanswer"), "answer");
    assert_eq!(strip_think("prefix reasoning</think>answer"), "answer");
    assert_eq!(strip_think("plain"), "plain");
}

#[tokio::test]
async fn vllm_stream_emits_reasoning_deltas() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![
        ev(
            "response.output_text.delta",
            &json!({"delta": "<think>hmm"}),
        ),
        ev(
            "response.output_text.delta",
            &json!({"delta": "</think>Answer"}),
        ),
        completed(
            "<think>hmm</think>Answer",
            &json!({"input_tokens": 1, "output_tokens": 1}),
        ),
    ]));
    let events = run(&gw, req("vllm")).await;
    assert!(matches!(&events[0], LlmEvent::ReasoningDelta(t) if t == "hmm"));
    assert_eq!(texts(&events), "Answer");
    assert_eq!(last_completion(&events).output_text, "Answer");
}

// ── other adapters ─────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_completions_stream_translation() {
    let gw = FakeGateway::new();
    let chunk = |v: Value| format!("data: {v}\n\n");
    gw.push(ProxyScript::sse(vec![
        chunk(json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]})),
        chunk(json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "Hi"}}]})),
        chunk(json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": " there"}, "finish_reason": "stop"}]})),
        chunk(json!({"id": "chatcmpl-1", "choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 2,
            "prompt_tokens_details": {"cached_tokens": 4}, "completion_tokens_details": {"reasoning_tokens": 1}}})),
        "data: [DONE]\n\n".into(),
    ]));
    let events = run(&gw, req("chat")).await;
    assert_eq!(texts(&events), "Hi there");
    let c = last_completion(&events);
    assert_eq!(c.response_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(
        c.usage,
        Some(UsageTokens {
            input_tokens: 9,
            output_tokens: 2,
            cache_read_input_tokens: 4,
            cache_write_input_tokens: 0,
            reasoning_tokens: 1,
        })
    );
}

#[tokio::test]
async fn anthropic_stream_translation() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![
        ev("message_start", &json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 11, "output_tokens": 1, "cache_read_input_tokens": 3}}})),
        ev("content_block_start", &json!({"index": 0, "content_block": {"type": "server_tool_use", "name": "web_search"}})),
        ev("content_block_stop", &json!({"index": 0})),
        ev("content_block_start", &json!({"index": 1, "content_block": {"type": "text", "text": ""}})),
        ev("content_block_delta", &json!({"index": 1, "delta": {"type": "text_delta", "text": "Hello"}})),
        ev("content_block_delta", &json!({"index": 1, "delta": {"type": "citations_delta", "citation": {
            "type": "web_search_result_location", "url": "https://w.example", "title": "W", "cited_text": "cited"}}})),
        ev("content_block_stop", &json!({"index": 1})),
        ev("ping", &json!({"type": "ping"})),
        ev("message_delta", &json!({"delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 7}})),
        ev("message_stop", &json!({"type": "message_stop"})),
    ]));
    let events = run(&gw, req("anthropic")).await;
    assert!(matches!(&events[0], LlmEvent::ToolStart { name, .. } if name == "web_search"));
    assert!(matches!(&events[1], LlmEvent::ToolDone { name, .. } if name == "web_search"));
    assert_eq!(texts(&events), "Hello");
    let c = last_completion(&events);
    assert_eq!(c.response_id.as_deref(), Some("msg_1"));
    assert_eq!(c.incomplete_reason.as_deref(), Some("max_output_tokens"));
    let u = c.usage.unwrap();
    assert_eq!(
        (u.input_tokens, u.output_tokens, u.cache_read_input_tokens),
        (11, 7, 3)
    );
    assert_eq!(c.citations.len(), 1);
}

#[tokio::test]
async fn anthropic_error_event() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![ev(
        "error",
        &json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    )]));
    let f = last_failure(&run(&gw, req("anthropic")).await);
    assert_eq!(f.code, "provider_error");
    assert_eq!(f.message, "Overloaded");
}

// ── pre-stream / HTTP errors ───────────────────────────────────────────────

#[tokio::test]
async fn http_429_with_retry_after() {
    let gw = FakeGateway::new();
    let f = stream_err(
        &gw,
        ProxyScript::Response {
            status: 429,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("retry-after".into(), "7".into()),
            ],
            body: FakeBody::Bytes(br#"{"error":{"message":"Rate limit reached"}}"#.to_vec()),
            source: Some(ErrorSource::Upstream),
        },
    )
    .await;
    assert_eq!(f.code, "rate_limited");
    assert!(f.message.contains('7'), "{}", f.message);

    let f = stream_err(
        &gw,
        ProxyScript::json(429, &json!({"error": {"message": "x"}})),
    )
    .await;
    assert_eq!(f.code, "rate_limited");
}

#[tokio::test]
async fn http_500_is_sanitized_provider_error() {
    let gw = FakeGateway::new();
    let f = stream_err(
        &gw,
        ProxyScript::json(
            500,
            &json!({"error": {"message": "boom for resp_abc123 see https://status.example", "type": "server_error"}}),
        ),
    )
    .await;
    assert_eq!(f.code, "provider_error");
    assert_eq!(f.message, "boom for [provider_id] see [url]");
    assert!(!f.context_length_exceeded);

    let f = stream_err(
        &gw,
        ProxyScript::Response {
            status: 502,
            headers: vec![],
            body: FakeBody::Bytes(b"<html>bad gateway</html>".to_vec()),
            source: Some(ErrorSource::Upstream),
        },
    )
    .await;
    assert_eq!(f.code, "provider_error");
    assert_eq!(f.message, "Provider returned HTTP 502");
}

#[tokio::test]
async fn http_400_context_length_exceeded() {
    let gw = FakeGateway::new();
    let f = stream_err(
        &gw,
        ProxyScript::json(
            400,
            &json!({"error": {"code": "context_length_exceeded", "message": "Your input exceeds the context window"}}),
        ),
    )
    .await;
    assert_eq!(f.code, "provider_error");
    assert!(f.context_length_exceeded);
}

#[tokio::test]
async fn http_504_provider_body_vs_gateway_timeout() {
    let gw = FakeGateway::new();
    let f = stream_err(
        &gw,
        ProxyScript::json(
            504,
            &json!({"error": {"message": "upstream model timed out"}}),
        ),
    )
    .await;
    assert_eq!(f.code, "provider_error");
    assert_eq!(f.message, "upstream model timed out");

    let problem = json!({"type": "gts.cf.core.errors.err.v1~cf.core.errors.deadline_exceeded.v1~",
                         "title": "Deadline Exceeded", "status": 504, "detail": "request timeout"});
    let f = stream_err(&gw, ProxyScript::json(504, &problem)).await;
    assert_eq!(f.code, "provider_timeout");

    let f = stream_err(
        &gw,
        ProxyScript::Response {
            status: 504,
            headers: vec![("content-type".into(), "application/problem+json".into())],
            body: FakeBody::Bytes(serde_json::to_vec(&problem).unwrap()),
            source: Some(ErrorSource::Gateway),
        },
    )
    .await;
    assert_eq!(f.code, "provider_timeout");
}

#[tokio::test]
async fn gateway_errors() {
    let gw = FakeGateway::new();
    let f = stream_err(&gw, ProxyScript::Err(errs::deadline_exceeded())).await;
    assert_eq!(f.code, "provider_timeout");

    let f = stream_err(&gw, ProxyScript::Err(errs::unavailable())).await;
    assert_eq!(f.code, "provider_error");
    assert!(!f.message.contains("http"), "{}", f.message);
}

#[tokio::test]
async fn unknown_provider_fails_pre_stream() {
    let gw = FakeGateway::new();
    let r = client(&gw)
        .stream(req("nope"), CancellationToken::new())
        .await;
    assert_eq!(r.err().unwrap().code, "provider_error");
    assert!(gw.proxy_calls().is_empty());
}

// ── complete() ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn complete_parses_json_response() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::json(
        200,
        &json!({"id": "resp_s", "status": "completed", "output": [
            {"type": "reasoning"},
            {"type": "message", "content": [
                {"type": "output_text", "text": "<summary>A</summary>"},
                {"type": "output_text", "text": " more"}
            ]}
        ], "usage": {"input_tokens": 500, "output_tokens": 40, "output_tokens_details": {"reasoning_tokens": 3}}}),
    ));
    let mut r = req("openai");
    r.request_type = RequestType::Summary;
    let res = client(&gw).complete(r).await.unwrap();
    assert_eq!(res.text, "<summary>A</summary> more");
    assert_eq!(res.response_id.as_deref(), Some("resp_s"));
    let u = res.usage.unwrap();
    assert_eq!(
        (u.input_tokens, u.output_tokens, u.reasoning_tokens),
        (500, 40, 3)
    );
    assert_eq!(gw.last_json_body()["stream"], false);
}

#[tokio::test]
async fn complete_maps_errors() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::json(
        400,
        &json!({"error": {"code": "context_length_exceeded", "message": "too long"}}),
    ));
    let f = client(&gw).complete(req("openai")).await.unwrap_err();
    assert!(f.context_length_exceeded);

    gw.push(ProxyScript::json(
        200,
        &json!({"id": "r", "status": "failed", "error": {"message": "nope"}}),
    ));
    let f = client(&gw).complete(req("openai")).await.unwrap_err();
    assert_eq!(f.message, "nope");

    gw.push(ProxyScript::Err(errs::deadline_exceeded()));
    let f = client(&gw).complete(req("openai")).await.unwrap_err();
    assert_eq!(f.code, "provider_timeout");
}

// ── FileStorage ────────────────────────────────────────────────────────────

#[tokio::test]
async fn upload_file_sends_multipart() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::json(
        200,
        &json!({"id": "file-xyz", "object": "file"}),
    ));
    let data = bytes::Bytes::from_static(b"%PDF-1.4 binary\r\n--not-a-boundary\x00\xff");
    let id = client(&gw)
        .upload_file(
            "openai",
            TENANT,
            "Q3 \"report\".pdf",
            "application/pdf",
            data.clone(),
        )
        .await
        .unwrap();
    assert_eq!(id, "file-xyz");

    let (method, uri, headers, body) = gw.proxy_calls().pop().unwrap();
    assert_eq!(method, http::Method::POST);
    assert_eq!(uri, "/api.openai.com/v1/files");
    let ct = headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(ct.starts_with("multipart/form-data; boundary="));
    let boundary = multer::parse_boundary(&ct).unwrap();
    let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(body) });
    let mut mp = multer::Multipart::new(stream, boundary);
    let purpose = mp.next_field().await.unwrap().unwrap();
    assert_eq!(purpose.name(), Some("purpose"));
    assert_eq!(purpose.text().await.unwrap(), "assistants");
    let file = mp.next_field().await.unwrap().unwrap();
    assert_eq!(file.name(), Some("file"));
    assert_eq!(file.file_name(), Some("Q3 %22report%22.pdf"));
    assert_eq!(
        file.content_type().unwrap().essence_str(),
        "application/pdf"
    );
    assert_eq!(file.bytes().await.unwrap(), data);
    assert!(mp.next_field().await.unwrap().is_none());
}

#[tokio::test]
async fn upload_file_errors() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::json(
        400,
        &json!({"error": {"message": "upload failed for file-abcdefghijklmnopqrstu"}}),
    ));
    let e = client(&gw)
        .upload_file(
            "openai",
            TENANT,
            "a.txt",
            "text/plain",
            bytes::Bytes::from_static(b"x"),
        )
        .await
        .unwrap_err();
    match e {
        StorageError::Http { status, message } => {
            assert_eq!(status, 400);
            assert_eq!(message, "upload failed for [provider_id]");
        }
        other => panic!("unexpected {other:?}"),
    }

    gw.push(ProxyScript::Err(errs::unavailable()));
    let e = client(&gw)
        .upload_file(
            "openai",
            TENANT,
            "a.txt",
            "text/plain",
            bytes::Bytes::from_static(b"x"),
        )
        .await
        .unwrap_err();
    assert!(matches!(e, StorageError::Transport(_)));
    assert!(e.is_transient());

    let e = client(&gw)
        .upload_file(
            "missing",
            TENANT,
            "a.txt",
            "text/plain",
            bytes::Bytes::from_static(b"x"),
        )
        .await
        .unwrap_err();
    assert!(matches!(e, StorageError::Config(_)));
}

#[tokio::test]
async fn vector_store_calls_openai() {
    let gw = FakeGateway::new();
    let c = client(&gw);
    gw.push(ProxyScript::json(
        200,
        &json!({"id": "vs_123", "object": "vector_store"}),
    ));
    assert_eq!(
        c.create_vector_store("openai", TENANT, "chat-1")
            .await
            .unwrap(),
        "vs_123"
    );
    assert_eq!(gw.last_json_body(), json!({"name": "chat-1"}));

    gw.push(ProxyScript::json(
        200,
        &json!({"id": "file-1", "status": "in_progress"}),
    ));
    let mut attrs = BTreeMap::new();
    attrs.insert("attachment_id".to_owned(), "a1".to_owned());
    assert_eq!(
        c.add_file_to_vector_store("openai", TENANT, "vs_123", "file-1", attrs)
            .await
            .unwrap(),
        VectorFileStatus::InProgress
    );
    assert_eq!(
        gw.last_json_body(),
        json!({"file_id": "file-1", "attributes": {"attachment_id": "a1"}})
    );

    for (body, expected) in [
        (json!({"id": "file-1"}), VectorFileStatus::InProgress),
        (json!({"status": "completed"}), VectorFileStatus::Completed),
        (
            json!({"status": "failed"}),
            VectorFileStatus::Failed("failed".into()),
        ),
        (
            json!({"status": "cancelled"}),
            VectorFileStatus::Failed("cancelled".into()),
        ),
    ] {
        gw.push(ProxyScript::json(200, &body));
        assert_eq!(
            c.get_vector_store_file_status("openai", TENANT, "vs_123", "file-1")
                .await
                .unwrap(),
            expected
        );
    }

    gw.push(ProxyScript::json(200, &json!({"deleted": true})));
    c.delete_vector_store("openai", TENANT, "vs_123")
        .await
        .unwrap();
    gw.push(ProxyScript::json(
        404,
        &json!({"error": {"message": "No such file"}}),
    ));
    let e = c.delete_file("openai", TENANT, "file-1").await.unwrap_err();
    assert!(e.is_not_found());

    let calls: Vec<(http::Method, String)> = gw
        .proxy_calls()
        .into_iter()
        .map(|(m, u, _, _)| (m, u))
        .collect();
    assert_eq!(
        calls,
        vec![
            (
                http::Method::POST,
                "/api.openai.com/v1/vector_stores".to_owned()
            ),
            (
                http::Method::POST,
                "/api.openai.com/v1/vector_stores/vs_123/files".to_owned()
            ),
            (
                http::Method::GET,
                "/api.openai.com/v1/vector_stores/vs_123/files/file-1".to_owned()
            ),
            (
                http::Method::GET,
                "/api.openai.com/v1/vector_stores/vs_123/files/file-1".to_owned()
            ),
            (
                http::Method::GET,
                "/api.openai.com/v1/vector_stores/vs_123/files/file-1".to_owned()
            ),
            (
                http::Method::GET,
                "/api.openai.com/v1/vector_stores/vs_123/files/file-1".to_owned()
            ),
            (
                http::Method::DELETE,
                "/api.openai.com/v1/vector_stores/vs_123".to_owned()
            ),
            (
                http::Method::DELETE,
                "/api.openai.com/v1/files/file-1".to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn storage_calls_azure_prefix_and_query() {
    let gw = FakeGateway::new();
    let c = client(&gw);
    gw.push(ProxyScript::json(200, &json!({"id": "assistant-1"})));
    c.upload_file(
        "azure",
        TENANT,
        "a.txt",
        "text/plain",
        bytes::Bytes::from_static(b"x"),
    )
    .await
    .unwrap();
    gw.push(ProxyScript::json(200, &json!({})));
    c.delete_file("azure", OTHER_TENANT, "assistant-1")
        .await
        .unwrap();
    let uris: Vec<String> = gw.proxy_calls().into_iter().map(|(_, u, _, _)| u).collect();
    assert_eq!(
        uris,
        vec![
            "/res.openai.azure.com/openai/files?api-version=2025-04-01-preview".to_owned(),
            "/other.openai.azure.com/openai/files/assistant-1?api-version=2025-04-01-preview"
                .to_owned(),
        ]
    );
}

// ── function tools / knowledge search ──────────────────────────────────────

use crate::infra::llm::{KnowledgeRetriever, ToolExchange};

fn search_tool() -> ToolSpec {
    ToolSpec::Function {
        name: "search_knowledge".into(),
        description: "Search".into(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    }
}

fn exchange() -> ToolExchange {
    ToolExchange {
        call_id: "call_1".into(),
        name: "search_knowledge".into(),
        arguments: r#"{"query":"q"}"#.into(),
        output: r#"{"results":[]}"#.into(),
    }
}

fn function_calls(events: &[LlmEvent]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::FunctionCall {
                call_id,
                name,
                arguments,
            } => Some((call_id.clone(), name.clone(), arguments.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn responses_request_with_function_tool_and_exchanges() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![completed("", &json!({}))]));
    let mut r = req("openai");
    r.tools = vec![search_tool()];
    r.max_tool_calls = None;
    r.tool_exchanges = vec![exchange()];
    run(&gw, r).await;
    let body = gw.last_json_body();
    assert_eq!(
        body["tools"],
        json!([{"type": "function", "name": "search_knowledge", "description": "Search",
                "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}])
    );
    assert!(body.get("max_tool_calls").is_none());
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(
        input[1],
        json!({"type": "function_call", "call_id": "call_1", "name": "search_knowledge", "arguments": "{\"query\":\"q\"}"})
    );
    assert_eq!(
        input[2],
        json!({"type": "function_call_output", "call_id": "call_1", "output": "{\"results\":[]}"})
    );
}

#[tokio::test]
async fn chat_completions_request_keeps_function_tools() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec!["data: [DONE]\n\n".into()]));
    let mut r = req("chat");
    r.tools = vec![
        ToolSpec::WebSearch {
            search_context_size: "low".into(),
        },
        search_tool(),
    ];
    r.tool_exchanges = vec![exchange()];
    run(&gw, r).await;
    let body = gw.last_json_body();
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "search_knowledge");
    let msgs = body["messages"].as_array().unwrap();
    let n = msgs.len();
    assert_eq!(msgs[n - 2]["role"], "assistant");
    assert_eq!(msgs[n - 2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(msgs[n - 2]["tool_calls"][0]["function"]["arguments"], "{\"query\":\"q\"}");
    assert_eq!(msgs[n - 1], json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"results\":[]}"}));
}

#[tokio::test]
async fn anthropic_request_with_function_tool_and_exchanges() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![ev("message_stop", &json!({"type": "message_stop"}))]));
    let mut r = req("anthropic");
    r.tools = vec![search_tool()];
    r.tool_exchanges = vec![exchange()];
    run(&gw, r).await;
    let body = gw.last_json_body();
    assert_eq!(body["tools"][0]["name"], "search_knowledge");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(
        msgs[1],
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "search_knowledge", "input": {"query": "q"}}]})
    );
    assert_eq!(
        msgs[2],
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "{\"results\":[]}"}]})
    );
}

#[tokio::test]
async fn responses_function_calls_are_reported_once() {
    let gw = FakeGateway::new();
    let fc = |id: &str| json!({"type": "function_call", "id": format!("fc_{id}"), "call_id": id,
                              "name": "search_knowledge", "arguments": "{\"query\":\"x\"}", "status": "completed"});
    gw.push(ProxyScript::sse(vec![
        ev("response.output_item.added", &json!({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "c1"}})),
        ev("response.function_call_arguments.delta", &json!({"type": "response.function_call_arguments.delta", "delta": "{"})),
        ev("response.output_item.done", &json!({"type": "response.output_item.done", "item": fc("c1")})),
        ev("response.completed", &json!({"type": "response.completed", "response": {
            "id": "resp_1", "status": "completed", "output": [fc("c1"), fc("c2")],
            "usage": {"input_tokens": 3, "output_tokens": 2}}})),
    ]));
    let events = run(&gw, req("openai")).await;
    assert_eq!(
        function_calls(&events),
        vec![
            ("c1".into(), "search_knowledge".into(), "{\"query\":\"x\"}".into()),
            ("c2".into(), "search_knowledge".into(), "{\"query\":\"x\"}".into()),
        ]
    );
    assert!(!events.iter().any(|e| matches!(e, LlmEvent::ToolStart { .. } | LlmEvent::ToolDone { .. })));
    assert!(matches!(events.last(), Some(LlmEvent::Completed(_))));
}

#[tokio::test]
async fn chat_completions_function_call_translation() {
    let gw = FakeGateway::new();
    let chunk = |v: Value| format!("data: {v}\n\n");
    gw.push(ProxyScript::sse(vec![
        chunk(json!({"id": "c-1", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call_9", "type": "function", "function": {"name": "search_knowledge", "arguments": ""}}]}}]})),
        chunk(json!({"id": "c-1", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "{\"query\":"}}]}}]})),
        chunk(json!({"id": "c-1", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "\"y\"}"}}]}, "finish_reason": "tool_calls"}]})),
        "data: [DONE]\n\n".into(),
    ]));
    let events = run(&gw, req("chat")).await;
    assert!(matches!(&events[0], LlmEvent::ToolStart { name, details }
        if name == "function_call" && details["call_id"] == "call_9" && details["name"] == "search_knowledge"));
    let done = events
        .iter()
        .find_map(|e| match e {
            LlmEvent::ToolDone { name, details } if name == "function_call" => Some(details.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(done, json!({"call_id": "call_9", "name": "search_knowledge", "arguments": "{\"query\":\"y\"}"}));
    assert_eq!(
        function_calls(&events),
        vec![("call_9".into(), "search_knowledge".into(), "{\"query\":\"y\"}".into())]
    );
    assert!(matches!(events.last(), Some(LlmEvent::Completed(_))));
}

#[tokio::test]
async fn anthropic_tool_use_translation() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::sse(vec![
        ev("message_start", &json!({"type": "message_start", "message": {"id": "msg_2", "usage": {"input_tokens": 5}}})),
        ev("content_block_start", &json!({"index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {}}})),
        ev("content_block_delta", &json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": "}})),
        ev("content_block_delta", &json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"z\"}"}})),
        ev("content_block_stop", &json!({"index": 0})),
        ev("message_delta", &json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 4}})),
        ev("message_stop", &json!({"type": "message_stop"})),
    ]));
    let events = run(&gw, req("anthropic")).await;
    assert!(matches!(&events[0], LlmEvent::ToolStart { name, .. } if name == "search_knowledge"));
    assert_eq!(
        function_calls(&events),
        vec![("toolu_1".into(), "search_knowledge".into(), "{\"query\": \"z\"}".into())]
    );
    assert!(!events.iter().any(|e| matches!(e, LlmEvent::ToolDone { .. })));
    assert!(matches!(events.last(), Some(LlmEvent::Completed(_))));
}

#[tokio::test]
async fn knowledge_retriever_calls_azure_search() {
    let gw = FakeGateway::new();
    let c = client(&gw);
    gw.push(ProxyScript::json(
        200,
        &json!({"object": "vector_store.search_results.page", "data": [
            {"file_id": "assistant-1", "filename": "kb.md", "score": 0.8,
             "content": [{"type": "text", "text": "chunk one"}, {"type": "text", "text": "more"}]},
            {"file_id": "assistant-2", "filename": "empty.md", "score": 0.1, "content": []}
        ]}),
    ));
    let chunks = c
        .search("azure", OTHER_TENANT, "vs_kb", "what?", 3)
        .await
        .unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "chunk one\nmore");
    assert_eq!(chunks[0].filename.as_deref(), Some("kb.md"));
    let (method, uri, _, body) = gw.proxy_calls().pop().unwrap();
    assert_eq!(method, http::Method::POST);
    assert_eq!(
        uri,
        "/other.openai.azure.com/openai/vector_stores/vs_kb/search?api-version=2025-04-01-preview"
    );
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({"query": "what?", "max_num_results": 3}));

    gw.push(ProxyScript::json(500, &json!({"error": {"message": "boom"}})));
    let e = c.search("azure", TENANT, "vs_kb", "q", 1).await.unwrap_err();
    assert!(matches!(e, StorageError::Http { status: 500, .. }), "{e:?}");
    // A provider without api_version is a configuration error.
    let e = c.search("vllm", TENANT, "vs_kb", "q", 1).await.unwrap_err();
    assert!(matches!(e, StorageError::Config(_)), "{e:?}");
}

#[tokio::test]
async fn json_body_function_calls_are_reported() {
    let gw = FakeGateway::new();
    gw.push(ProxyScript::json(
        200,
        &json!({"id": "resp_j2", "status": "completed",
                "output": [{"type": "function_call", "call_id": "c7", "name": "search_knowledge", "arguments": "{}"}],
                "usage": {"input_tokens": 3, "output_tokens": 4}}),
    ));
    let events = run(&gw, req("openai")).await;
    assert_eq!(
        function_calls(&events),
        vec![("c7".into(), "search_knowledge".into(), "{}".into())]
    );
    assert!(matches!(events.last(), Some(LlmEvent::Completed(_))));
}
