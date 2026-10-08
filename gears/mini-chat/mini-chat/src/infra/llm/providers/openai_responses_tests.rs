#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use mini_chat_sdk::{ModelApiParams, UsageTokens};
use oagw_sdk::api::ErrorSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::config::ProviderKind;
use crate::infra::llm::fake_gw::{FakeGw, Reply, client, http_error, sse, sse_raw};
use crate::infra::llm::types::{
    ContentPart, InputItem, LlmClient, LlmEvent, LlmRequest, ProviderError, RawCitation,
    RequestMetadata, ResolvedProvider, ToolSpec, feature_label, provider_user,
};

const TENANT: &str = "6f1d7a52-0f6e-4c37-9a3c-0d7a8f3c2b11";
const USER: &str = "0b9e4c1d-2f3a-4b5c-8d6e-7f8091a2b3c4";

#[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
struct ProxyErr;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn provider() -> ResolvedProvider {
    ResolvedProvider {
        provider_id: "openai".to_owned(),
        kind: ProviderKind::OpenaiResponses,
        alias: "api.openai.com".to_owned(),
        api_path: "/v1/responses".to_owned(),
        storage: None,
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "gpt-5-mini".to_owned(),
        instructions: "be helpful".to_owned(),
        input: vec![
            InputItem::Message {
                role: "user",
                content: vec![
                    ContentPart::InputText("what is this?".to_owned()),
                    ContentPart::InputImage {
                        file_id: "file-abc123".to_owned(),
                    },
                ],
            },
            InputItem::Message {
                role: "assistant",
                content: vec![ContentPart::OutputText("a cat".to_owned())],
            },
        ],
        tools: vec![],
        max_output_tokens: 1024,
        api_params: ModelApiParams::default(),
        max_tool_calls: Some(2),
        user: provider_user(TENANT, USER),
        metadata: RequestMetadata {
            tenant_id: TENANT.to_owned(),
            user_id: USER.to_owned(),
            chat_id: "c0ffee00-0000-4000-8000-000000000001".to_owned(),
            request_type: "chat",
            feature: "none".to_owned(),
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

async fn stream_err(reply: Reply) -> ProviderError {
    let gw = FakeGw::with(reply);
    let (client, _) = client(&gw);
    match client
        .stream(&provider(), request(), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("expected an error"),
        Err(e) => e,
    }
}

fn usage_json() -> Value {
    json!({
        "input_tokens": 120,
        "output_tokens": 40,
        "input_tokens_details": {"cached_tokens": 100},
        "output_tokens_details": {"reasoning_tokens": 8},
        "total_tokens": 160
    })
}

fn usage() -> UsageTokens {
    UsageTokens {
        input_tokens: 120,
        output_tokens: 40,
        cache_read_input_tokens: 100,
        cache_write_input_tokens: 0,
        reasoning_tokens: 8,
    }
}

fn completed(output: &Value) -> (&'static str, Value) {
    (
        "response.completed",
        json!({"type": "response.completed", "response": {
            "id": "resp_123", "status": "completed", "output": output, "usage": usage_json()
        }}),
    )
}

fn tool(name: &str, details: Value, start: bool) -> LlmEvent {
    if start {
        LlmEvent::ToolStart {
            name: name.to_owned(),
            details,
        }
    } else {
        LlmEvent::ToolDone {
            name: name.to_owned(),
            details,
        }
    }
}

// ── Request ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn builds_request_body() {
    let gw = FakeGw::with(sse(&[completed(&json!([]))]));
    let (client, ctx) = client(&gw);
    let mut req = request();
    req.tools = vec![
        ToolSpec::CodeInterpreter {
            file_ids: vec!["file-xlsx000000001".to_owned()],
        },
        ToolSpec::FileSearch {
            vector_store_ids: vec!["vs_abc".to_owned()],
            max_num_results: 5,
        },
        ToolSpec::WebSearch {
            search_context_size: "low".to_owned(),
        },
        ToolSpec::Function {
            name: "search_knowledge".to_owned(),
            description: "kb".to_owned(),
            parameters: json!({"type": "object"}),
        },
    ];
    req.metadata.feature = feature_label(&req.tools);
    req.api_params.top_p = Some(0.5);
    req.api_params.reasoning_effort = Some("low".to_owned());
    req.api_params.extra_body = Some(
        json!({"service_tier": "flex", "model": "evil", "stream": false, "user": "x", "metadata": {}})
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
    assert_eq!(cap.method, http::Method::POST);
    assert_eq!(cap.uri, "/api.openai.com/v1/responses");
    assert_eq!(cap.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        cap.subject_id,
        ctx.subject_id(),
        "proxied with the service identity"
    );
    let body = cap.body;

    let user = body["user"].as_str().unwrap();
    assert_eq!(user.len(), 64);
    assert!(user.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(
        user,
        "6f1d7a520f6e4c379a3c0d7a8f3c2b110b9e4c1d2f3a4b5c8d6e7f8091a2b3c4"
    );

    assert_eq!(body["model"], "gpt-5-mini");
    assert_eq!(body["instructions"], "be helpful");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_output_tokens"], 1024);
    assert_eq!(body["max_tool_calls"], 2);
    assert_eq!(
        body["input"],
        json!([
            {"role": "user", "content": [
                {"type": "input_text", "text": "what is this?"},
                {"type": "input_image", "file_id": "file-abc123"}
            ]},
            {"role": "assistant", "content": [{"type": "output_text", "text": "a cat"}]}
        ])
    );
    assert_eq!(
        body["tools"],
        json!([
            {"type": "code_interpreter", "container": {"type": "auto", "file_ids": ["file-xlsx000000001"]}},
            {"type": "file_search", "vector_store_ids": ["vs_abc"], "max_num_results": 5},
            {"type": "web_search", "search_context_size": "low"},
            {"type": "function", "name": "search_knowledge", "description": "kb", "parameters": {"type": "object"}}
        ])
    );
    assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(
        body["metadata"],
        json!({
            "tenant_id": TENANT,
            "user_id": USER,
            "chat_id": "c0ffee00-0000-4000-8000-000000000001",
            "request_type": "chat",
            "feature": "file_search+web_search+code_interpreter"
        })
    );
    assert_eq!(body["metadata"]["request_type"], "chat");
    // api params only when set
    assert!(body.get("temperature").is_none());
    assert!(body.get("frequency_penalty").is_none());
    assert!(body.get("presence_penalty").is_none());
    assert_eq!(body["top_p"], 0.5);
    assert_eq!(body["reasoning"], json!({"effort": "low"}));
    // extra_body merged, controlled keys ignored
    assert_eq!(body["service_tier"], "flex");
}

#[tokio::test]
async fn minimal_body_has_no_tools_or_include() {
    let gw = FakeGw::with(sse(&[completed(&json!([]))]));
    let (client, _) = client(&gw);
    let mut req = request();
    req.max_tool_calls = None;
    req.api_params.temperature = Some(0.2);
    let stream = client
        .stream(&provider(), req, CancellationToken::new())
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;
    let body = gw.last().body;
    assert!(body.get("tools").is_none());
    assert!(body.get("include").is_none());
    assert!(body.get("max_tool_calls").is_none());
    assert!(body.get("reasoning").is_none());
    assert_eq!(body["temperature"], 0.2);
}

#[test]
fn feature_label_orders_builtin_tools() {
    let fs = ToolSpec::FileSearch {
        vector_store_ids: vec![],
        max_num_results: 1,
    };
    let ws = ToolSpec::WebSearch {
        search_context_size: "low".to_owned(),
    };
    let ci = ToolSpec::CodeInterpreter { file_ids: vec![] };
    let func = ToolSpec::Function {
        name: "f".to_owned(),
        description: String::new(),
        parameters: json!({}),
    };
    assert_eq!(feature_label(&[]), "none");
    assert_eq!(feature_label(std::slice::from_ref(&func)), "none");
    assert_eq!(
        feature_label(&[ws.clone(), fs.clone()]),
        "file_search+web_search"
    );
    assert_eq!(
        feature_label(&[ci.clone(), fs]),
        "file_search+code_interpreter"
    );
    assert_eq!(
        feature_label(&[ci, func, ws]),
        "web_search+code_interpreter"
    );
}

#[test]
fn provider_user_falls_back_for_non_uuid() {
    assert_eq!(provider_user("t1", USER), format!("t1:{USER}"));
}

#[tokio::test]
async fn azure_api_path_query_preserved() {
    let gw = FakeGw::with(sse(&[completed(&json!([]))]));
    let (client, _) = client(&gw);
    let mut p = provider();
    p.alias = "my-res.openai.azure.com".to_owned();
    p.api_path = "/openai/deployments/{model}/responses?api-version=2025-04-01-preview".to_owned();
    let stream = client
        .stream(&p, request(), CancellationToken::new())
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;
    assert_eq!(
        gw.last().uri,
        "/my-res.openai.azure.com/openai/deployments/gpt-5-mini/responses?api-version=2025-04-01-preview"
    );
}

// ── Event translation ────────────────────────────────────────────────────────

#[tokio::test]
async fn translates_text_and_completion_usage() {
    let events = run(sse(&[
        ("response.created", json!({"type": "response.created", "response": {"id": "resp_123"}})),
        ("response.output_text.delta", json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "Hel"})),
        ("response.output_text.delta", json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "lo"})),
        completed(&json!([])),
    ]))
    .await;
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("Hel".to_owned()),
            LlmEvent::TextDelta("lo".to_owned()),
            LlmEvent::Completed {
                usage: Some(usage()),
                response_id: Some("resp_123".to_owned()),
                incomplete_reason: None,
            },
        ]
    );
}

#[tokio::test]
async fn uses_type_field_when_event_line_missing() {
    let body = format!(
        "data: {}\n\nevent: message\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"type": "response.output_text.delta", "delta": "hi"}),
        json!({"type": "response.completed", "response": {"id": "resp_1", "usage": null}})
    );
    let events = run(sse_raw(&body)).await;
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("hi".to_owned()),
            LlmEvent::Completed {
                usage: None,
                response_id: Some("resp_1".to_owned()),
                incomplete_reason: None,
            },
        ]
    );
}

#[tokio::test]
async fn file_search_events_and_files_searched() {
    let events = run(sse(&[
        (
            "response.file_search_call.in_progress",
            json!({"type": "response.file_search_call.in_progress"}),
        ),
        (
            "response.file_search_call.searching",
            json!({"type": "response.file_search_call.searching", "item_id": "fs_1"}),
        ),
        (
            "response.file_search_call.completed",
            json!({"type": "response.file_search_call.completed", "item_id": "fs_1"}),
        ),
        (
            "response.file_search_call.searching",
            json!({"type": "response.file_search_call.searching"}),
        ),
        (
            "response.file_search_call.completed",
            json!({"type": "response.file_search_call.completed", "results": [{}, {}, {}]}),
        ),
    ]))
    .await;
    assert_eq!(
        events,
        vec![
            tool("file_search", json!({}), true),
            tool("file_search", json!({"files_searched": 0}), false),
            tool("file_search", json!({}), true),
            tool("file_search", json!({"files_searched": 3}), false),
        ]
    );
}

#[tokio::test]
async fn web_search_tool_events() {
    let events = run(sse(&[
        (
            "response.web_search_call.in_progress",
            json!({"type": "response.web_search_call.in_progress"}),
        ),
        (
            "response.web_search_call.searching",
            json!({"type": "response.web_search_call.searching"}),
        ),
        (
            "response.web_search_call.completed",
            json!({"type": "response.web_search_call.completed"}),
        ),
    ]))
    .await;
    assert_eq!(
        events,
        vec![
            tool("web_search", json!({}), true),
            tool("web_search", json!({}), false)
        ]
    );
}

#[tokio::test]
async fn code_interpreter_done_output_truncated() {
    let long = "x".repeat(8190);
    let events = run(sse(&[
        ("response.code_interpreter_call.in_progress", json!({"type": "response.code_interpreter_call.in_progress"})),
        ("response.code_interpreter_call.interpreting", json!({"type": "response.code_interpreter_call.interpreting"})),
        ("response.code_interpreter_call.completed", json!({"type": "response.code_interpreter_call.completed"})),
        ("response.output_item.done", json!({"type": "response.output_item.done", "item": {
            "type": "code_interpreter_call", "id": "ci_1",
            "outputs": [{"type": "logs", "logs": "line1"}, {"type": "image", "url": "x"}, {"type": "logs", "logs": long}]
        }})),
        ("response.output_item.done", json!({"type": "response.output_item.done", "item": {
            "type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": "short"}]
        }})),
        ("response.output_item.done", json!({"type": "response.output_item.done", "item": {"type": "message", "content": []}})),
    ]))
    .await;
    // "line1\n" + 8190 x = 8196 chars -> first 8192 chars + suffix.
    let expected = format!("line1\n{}...[truncated]", "x".repeat(8186));
    assert_eq!(
        events,
        vec![
            tool("code_interpreter", json!({}), true),
            tool("code_interpreter", json!({"output": expected}), false),
            tool("code_interpreter", json!({"output": "short"}), false),
        ]
    );
}

#[tokio::test]
async fn web_citation_snippet_from_range() {
    // Range applied as character offsets (the text has a multi-byte char).
    let text = "Caf\u{e9} growth is 5% per year.";
    let events = run(sse(&[
        ("response.output_text.delta", json!({"type": "response.output_text.delta", "output_index": 1, "content_index": 0, "delta": text})),
        ("response.output_text.annotation.added", json!({
            "type": "response.output_text.annotation.added", "output_index": 1, "content_index": 0, "annotation_index": 0,
            "annotation": {"type": "url_citation", "url": "https://ex.com/a", "title": "A", "start_index": 5, "end_index": 14}
        })),
        completed(&json!([
            {"type": "web_search_call", "id": "ws_1"},
            {"type": "message", "content": [{"type": "output_text", "text": text, "annotations": [
                {"type": "url_citation", "url": "https://ex.com/a", "title": "A", "start_index": 5, "end_index": 14},
                {"type": "url_citation", "url": "https://ex.com/b", "title": "B", "start_index": 90, "end_index": 99},
                {"type": "url_citation", "url": "https://ex.com/c", "title": "C", "text": "own text"}
            ]}]}
        ])),
    ]))
    .await;
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(
        events[1],
        LlmEvent::Citations(vec![
            RawCitation::Web {
                url: "https://ex.com/a".to_owned(),
                title: "A".to_owned(),
                snippet: "growth is".to_owned(),
                span: Some((5, 14)),
            },
            RawCitation::Web {
                url: "https://ex.com/b".to_owned(),
                title: "B".to_owned(),
                snippet: String::new(),
                span: Some((90, 99)),
            },
            RawCitation::Web {
                url: "https://ex.com/c".to_owned(),
                title: "C".to_owned(),
                snippet: "own text".to_owned(),
                span: None,
            },
        ])
    );
    assert!(matches!(events[2], LlmEvent::Completed { .. }));
}

#[tokio::test]
async fn file_citation_maps_raw_id() {
    let events = run(sse(&[
        ("response.output_text.delta", json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "See report."})),
        ("response.output_text.annotation.added", json!({
            "type": "response.output_text.annotation.added", "output_index": 0, "content_index": 0, "annotation_index": 0,
            "annotation": {"type": "file_citation", "file_id": "file-AbCdEf123456", "filename": "q3.pdf", "index": 10}
        })),
        // Final output without annotations: the streamed annotation still counts.
        completed(&json!([])),
    ]))
    .await;
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("See report.".to_owned()),
            LlmEvent::Citations(vec![RawCitation::File {
                provider_file_id: "file-AbCdEf123456".to_owned(),
                filename: Some("q3.pdf".to_owned()),
            }]),
            LlmEvent::Completed {
                usage: Some(usage()),
                response_id: Some("resp_123".to_owned()),
                incomplete_reason: None,
            },
        ]
    );
}

#[tokio::test]
async fn response_failed_keeps_usage_and_sanitizes() {
    let events = run(sse(&[
        ("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": "par"})),
        ("response.failed", json!({"type": "response.failed", "response": {
            "id": "resp_x", "status": "failed",
            "error": {"code": "server_error", "message": "bad resp_abc123 at https://x.y/z with sk-ABCDEFGHIJKL"},
            "usage": usage_json()
        }})),
    ]))
    .await;
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("par".to_owned()),
            LlmEvent::Failed {
                error: ProviderError {
                    code: "provider_error",
                    message: "bad [provider_id] at [url] with [credential]".to_owned(),
                    context_length_exceeded: false,
                    retry_after_secs: None,
                },
                usage: Some(usage()),
            },
        ]
    );
}

#[tokio::test]
async fn error_event_flat_and_unparseable() {
    let flat = run(sse(&[(
        "error",
        json!({"type": "error", "code": "context_length_exceeded", "message": "too long for msg_9"}),
    )]))
    .await;
    assert_eq!(
        flat,
        vec![LlmEvent::Failed {
            error: ProviderError {
                code: "provider_error",
                message: "too long for [provider_id]".to_owned(),
                context_length_exceeded: true,
                retry_after_secs: None,
            },
            usage: None,
        }]
    );

    let raw = run(sse_raw("event: error\ndata: upstream exploded\n\n")).await;
    assert_eq!(
        raw,
        vec![LlmEvent::Failed {
            error: ProviderError::provider("upstream exploded"),
            usage: None,
        }]
    );
}

#[tokio::test]
async fn incomplete_is_completed_with_reason() {
    let events = run(sse(&[
        ("response.output_text.annotation.added", json!({
            "type": "response.output_text.annotation.added", "output_index": 0, "content_index": 0, "annotation_index": 0,
            "annotation": {"type": "file_citation", "file_id": "file-AbCdEf123456", "filename": "q3.pdf", "index": 0}
        })),
        ("response.incomplete", json!({"type": "response.incomplete", "response": {
            "id": "resp_9", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"}, "usage": usage_json()
        }})),
    ]))
    .await;
    // No citations on an incomplete response.
    assert_eq!(
        events,
        vec![LlmEvent::Completed {
            usage: Some(usage()),
            response_id: Some("resp_9".to_owned()),
            incomplete_reason: Some("max_output_tokens".to_owned()),
        }]
    );
}

#[tokio::test]
async fn stream_end_without_terminal_event_just_ends() {
    let events = run(sse(&[(
        "response.output_text.delta",
        json!({"type": "response.output_text.delta", "delta": "a"}),
    )]))
    .await;
    assert_eq!(events, vec![LlmEvent::TextDelta("a".to_owned())]);
}

#[tokio::test]
async fn cancellation_ends_stream_and_drops_body() {
    let gw = FakeGw::with(Reply::Response {
        status: 200,
        headers: vec![("content-type", "text/event-stream".to_owned())],
        source: None,
        chunks: vec![Bytes::from(format!(
            "data: {}\n\n",
            json!({"type": "response.output_text.delta", "delta": "a"})
        ))],
        hang: true,
    });
    let (client, _) = client(&gw);
    let cancel = CancellationToken::new();
    let mut stream = client
        .stream(&provider(), request(), cancel.clone())
        .await
        .unwrap();
    assert_eq!(
        stream.next().await,
        Some(LlmEvent::TextDelta("a".to_owned()))
    );
    assert!(!gw.dropped());
    cancel.cancel();
    let next = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("stream ends after cancel");
    assert_eq!(next, None);
    assert!(gw.dropped(), "body dropped on cancel");
}

// ── HTTP / gateway errors ────────────────────────────────────────────────────

#[tokio::test]
async fn http_429_is_rate_limited_with_retry_after() {
    let err = stream_err(http_error(
        429,
        ErrorSource::Upstream,
        vec![("retry-after", "17".to_owned())],
        &json!({"error": {"message": "Rate limit reached", "code": "rate_limit_exceeded"}}),
    ))
    .await;
    assert_eq!(err.code, "rate_limited");
    assert_eq!(err.retry_after_secs, Some(17));
    assert!(err.message.contains("17"), "{}", err.message);

    let no_header = stream_err(http_error(429, ErrorSource::Upstream, vec![], &json!({}))).await;
    assert_eq!(no_header.code, "rate_limited");
    assert_eq!(no_header.retry_after_secs, None);
}

#[tokio::test]
async fn gateway_timeout_is_provider_timeout() {
    let err = stream_err(Reply::Error(
        ProxyErr::deadline_exceeded("upstream timed out").create(),
    ))
    .await;
    assert_eq!(err.code, "provider_timeout");

    let gw504 = stream_err(http_error(
        504,
        ErrorSource::Gateway,
        vec![("content-type", "application/problem+json".to_owned())],
        &json!({"title": "Gateway Timeout", "status": 504, "detail": "upstream read timed out"}),
    ))
    .await;
    assert_eq!(gw504.code, "provider_timeout");

    // The provider's own 504 with a JSON error body is a provider error.
    let upstream504 = stream_err(http_error(
        504,
        ErrorSource::Upstream,
        vec![],
        &json!({"error": {"message": "provider overloaded"}}),
    ))
    .await;
    assert_eq!(upstream504.code, "provider_error");
    assert_eq!(upstream504.message, "provider overloaded");
}

#[tokio::test]
async fn http_error_body_is_parsed_and_sanitized() {
    let err = stream_err(http_error(
        400,
        ErrorSource::Upstream,
        vec![],
        &json!({"error": {"code": "context_length_exceeded", "message": "Input for resp_abc is too long"}}),
    ))
    .await;
    assert_eq!(
        err,
        ProviderError {
            code: "provider_error",
            message: "Input for [provider_id] is too long".to_owned(),
            context_length_exceeded: true,
            retry_after_secs: None,
        }
    );

    let other = stream_err(Reply::Error(CanonicalError::internal("boom").create())).await;
    assert_eq!(other.code, "provider_error");
}

// ── Non-streaming ────────────────────────────────────────────────────────────

#[tokio::test]
async fn complete_returns_text_and_usage() {
    let gw = FakeGw::with(Reply::Response {
        status: 200,
        headers: vec![("content-type", "application/json".to_owned())],
        source: None,
        chunks: vec![Bytes::from(
            json!({"id": "resp_1", "status": "completed", "usage": usage_json(), "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "Sum"},
                    {"type": "output_text", "text": "mary"}
                ]}
            ]})
            .to_string(),
        )],
        hang: false,
    });
    let (client, _) = client(&gw);
    let mut req = request();
    req.stream = true;
    req.metadata.request_type = "summary";
    let out = client.complete(&provider(), req).await.unwrap();
    assert_eq!(out.text, "Summary");
    assert_eq!(out.usage, Some(usage()));
    let body = gw.last().body;
    assert_eq!(body["stream"], false);
    assert_eq!(body["metadata"]["request_type"], "summary");
}

#[tokio::test]
async fn complete_maps_http_errors() {
    let gw = FakeGw::with(http_error(
        500,
        ErrorSource::Upstream,
        vec![],
        &json!({"error": {"message": "oops vs_abcdefghijklmnop"}}),
    ));
    let (client, _) = client(&gw);
    let err = client.complete(&provider(), request()).await.unwrap_err();
    assert_eq!(err.code, "provider_error");
    assert_eq!(err.message, "oops [provider_id]");
}

// ── Function tools (knowledge search loop) ───────────────────────────────────

#[tokio::test]
async fn function_call_items_are_sent_as_input() {
    let gw = FakeGw::with(sse(&[completed(&json!([]))]));
    let (client, _) = client(&gw);
    let mut req = request();
    req.input.push(InputItem::FunctionCall {
        call_id: "call_1".to_owned(),
        name: "search_knowledge".to_owned(),
        arguments: r#"{"query":"q"}"#.to_owned(),
    });
    req.input.push(InputItem::FunctionCallOutput {
        call_id: "call_1".to_owned(),
        output: "chunks".to_owned(),
    });
    let stream = client
        .stream(&provider(), req, CancellationToken::new())
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;
    let input = gw.last().body["input"].clone();
    assert_eq!(
        input[2],
        json!({"type": "function_call", "call_id": "call_1", "name": "search_knowledge", "arguments": "{\"query\":\"q\"}"})
    );
    assert_eq!(
        input[3],
        json!({"type": "function_call_output", "call_id": "call_1", "output": "chunks"})
    );
}

#[tokio::test]
async fn function_call_item_done_is_a_function_call_event() {
    let events = run(sse(&[
        ("response.output_item.done", json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "search_knowledge",
            "arguments": "{\"query\":\"vacation\"}", "status": "completed"
        }})),
        completed(&json!([])),
    ]))
    .await;
    assert_eq!(
        events[0],
        LlmEvent::FunctionCall {
            call_id: "call_9".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: "{\"query\":\"vacation\"}".to_owned(),
        }
    );
    assert!(matches!(events[1], LlmEvent::Completed { .. }));
}
