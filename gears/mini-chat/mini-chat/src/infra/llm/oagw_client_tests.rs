#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::integer_division, clippy::non_ascii_literal)]
//! Slice 4 and 6: the OAGW provider client against the fake gateway:
//! the request as sent, the incremental reading of the body, the failure
//! classification and the extension seam for further adapters.

use std::future::Future;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::config::ProviderKind;
use crate::domain::service::stream::context::{ContextPlan, CurrentMessage};
use crate::domain::service::stream::ports::ProviderTarget;
use crate::domain::service::stream::tools::ToolsDecision;
use crate::domain::service::stream::types::ProviderEvent;
use crate::infra::llm::providers::{EventTranslator, ProviderAdapter};
use crate::infra::llm::sse_parse::SseEvent;
use crate::test_doubles::{FakeOagw, Fault, LogCapture, ProxyReply, user_ctx};
use tracing::Level;

async fn within<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("timed out")
}

fn built() -> BuiltRequest {
    BuiltRequest {
        kind: ProviderKind::OpenaiResponses,
        uri: "/api.openai.test/v1/responses?api-version=1".to_owned(),
        headers: vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Accept".to_owned(), "text/event-stream".to_owned()),
        ],
        body: json!({ "model": "m", "stream": true }),
    }
}

fn delta(text: &str) -> (&'static str, Value) {
    (
        "response.output_text.delta",
        json!({ "type": "response.output_text.delta", "delta": text }),
    )
}

fn completed() -> (&'static str, Value) {
    (
        "response.completed",
        json!({ "response": { "id": "resp_1", "usage": { "input_tokens": 3, "output_tokens": 4 } } }),
    )
}

async fn open(client: &OagwProviderClient) -> Result<Box<dyn ProviderStream>, ProviderFailure> {
    within(client.open(&user_ctx(), &built())).await
}

async fn failure_of(oagw: &std::sync::Arc<FakeOagw>) -> ProviderFailure {
    let client = OagwProviderClient::new(oagw.clone());
    match open(&client).await {
        Ok(_) => panic!("a failure expected"),
        Err(failure) => failure,
    }
}

async fn next(
    stream: &mut Box<dyn ProviderStream>,
) -> Option<Result<ProviderEvent, ProviderFailure>> {
    within(stream.next_event()).await
}

fn text(content: &str) -> ProviderEvent {
    ProviderEvent::TextDelta {
        text: content.to_owned(),
        item_id: None,
        content_index: None,
    }
}

// ---- the request --------------------------------------------------------------------------------

#[tokio::test]
async fn the_request_goes_through_proxy_request_with_the_callers_context() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(ProxyReply::sse(&[completed()]));
    let client = OagwProviderClient::new(oagw.clone());
    let _stream = open(&client).await.unwrap();
    let sent = oagw.proxied();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "POST");
    assert_eq!(sent[0].uri, "/api.openai.test/v1/responses?api-version=1");
    assert_eq!(sent[0].header("content-type"), Some("application/json"));
    assert_eq!(sent[0].header("accept"), Some("text/event-stream"));
    assert_eq!(sent[0].json(), json!({ "model": "m", "stream": true }));
    assert_eq!(sent[0].subject, user_ctx().subject_id());
    assert_eq!(sent[0].tenant, user_ctx().subject_tenant_id());
    assert!(
        sent[0].header("authorization").is_none(),
        "OAGW injects the credential"
    );
}

// ---- reading the body ---------------------------------------------------------------------------

#[tokio::test]
async fn a_complete_body_yields_its_events_in_order_and_then_the_end() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(ProxyReply::sse(&[delta("Hel"), delta("lo"), completed()]));
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    assert_eq!(next(&mut stream).await, Some(Ok(text("Hel"))));
    assert_eq!(next(&mut stream).await, Some(Ok(text("lo"))));
    let Some(Ok(ProviderEvent::Completed {
        usage,
        response_id,
        incomplete_reason,
    })) = next(&mut stream).await
    else {
        panic!("completion expected");
    };
    assert_eq!(usage.unwrap().input_tokens, 3);
    assert_eq!(response_id.as_deref(), Some("resp_1"));
    assert_eq!(incomplete_reason, None);
    assert_eq!(next(&mut stream).await, None);
}

#[tokio::test]
async fn each_event_is_available_before_the_next_chunk_is_released() {
    let oagw = FakeOagw::new();
    let feed = oagw.push_gated();
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    for n in 0..5 {
        feed.send_event(delta(&format!("d{n}")).0, &delta(&format!("d{n}")).1);
        assert_eq!(
            next(&mut stream).await,
            Some(Ok(text(&format!("d{n}")))),
            "event {n} must not wait for event {}",
            n + 1
        );
    }
    feed.send_event(completed().0, &completed().1);
    assert!(matches!(
        next(&mut stream).await,
        Some(Ok(ProviderEvent::Completed { .. }))
    ));
}

#[tokio::test]
async fn an_event_split_across_chunks_is_assembled() {
    let oagw = FakeOagw::new();
    let feed = oagw.push_gated();
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    let frame = crate::test_doubles::sse_frame(delta("héllo").0, &delta("héllo").1);
    let bytes = frame.as_bytes();
    let (left, right) = bytes.split_at(bytes.len() / 2 + 1);
    feed.send(left.to_vec());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), stream.next_event())
            .await
            .is_err(),
        "half an event is not an event"
    );
    feed.send(right.to_vec());
    assert_eq!(next(&mut stream).await, Some(Ok(text("héllo"))));
}

#[tokio::test]
async fn next_event_is_cancel_safe_a_dropped_wait_loses_nothing() {
    let oagw = FakeOagw::new();
    let feed = oagw.push_gated();
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    let frame = crate::test_doubles::sse_frame(delta("kept").0, &delta("kept").1);
    let (left, right) = frame.as_bytes().split_at(10);
    feed.send(left.to_vec());
    // The task races the read against the token and the liveness timer: a lost
    // race drops the pending future after part of an event was consumed.
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.next_event())
            .await
            .is_err()
    );
    feed.send(right.to_vec());
    assert_eq!(next(&mut stream).await, Some(Ok(text("kept"))));
}

#[tokio::test]
async fn several_events_in_one_chunk_are_yielded_one_by_one() {
    let oagw = FakeOagw::new();
    let feed = oagw.push_gated();
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    let body = format!(
        "{}{}{}",
        crate::test_doubles::sse_frame(delta("a").0, &delta("a").1),
        crate::test_doubles::sse_frame(delta("b").0, &delta("b").1),
        crate::test_doubles::sse_frame(delta("c").0, &delta("c").1),
    );
    feed.send(body.into_bytes());
    for expected in ["a", "b", "c"] {
        assert_eq!(next(&mut stream).await, Some(Ok(text(expected))));
    }
}

#[tokio::test]
async fn events_without_an_event_line_use_the_type_of_their_data() {
    let oagw = FakeOagw::new();
    let feed = oagw.push_gated();
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    feed.send_data(&json!({ "type": "response.output_text.delta", "delta": "typed" }));
    assert_eq!(next(&mut stream).await, Some(Ok(text("typed"))));
}

#[tokio::test]
async fn ignored_events_and_the_done_marker_do_not_end_the_stream() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(ProxyReply::sse_text(format!(
        "{}{}data: [DONE]\n\n{}",
        crate::test_doubles::sse_frame("response.created", &json!({})),
        ": keep-alive\n\n",
        crate::test_doubles::sse_frame(delta("after").0, &delta("after").1),
    )));
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    assert_eq!(next(&mut stream).await, Some(Ok(text("after"))));
    assert_eq!(next(&mut stream).await, None);
}

#[tokio::test]
async fn a_thousand_events_are_read_in_order() {
    let oagw = FakeOagw::new();
    let events: Vec<_> = (0..1000).map(|n| delta(&n.to_string())).collect();
    oagw.push_proxy(ProxyReply::sse(&events));
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    for n in 0..1000 {
        assert_eq!(next(&mut stream).await, Some(Ok(text(&n.to_string()))));
    }
    assert_eq!(next(&mut stream).await, None);
}

#[tokio::test]
async fn dropping_the_stream_drops_the_response_body() {
    let oagw = FakeOagw::new();
    let feed = oagw.push_gated();
    let client = OagwProviderClient::new(oagw.clone());
    let stream = open(&client).await.unwrap();
    assert!(!feed.is_dropped());
    drop(stream);
    within(feed.dropped()).await;
}

// ---- failures -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_gateway_deadline_is_a_provider_timeout() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(ProxyReply::Error(Fault::DeadlineExceeded));
    let failure = failure_of(&oagw).await;
    assert_eq!(failure.code, StreamErrorCode::ProviderTimeout);
}

#[tokio::test]
async fn another_gateway_error_is_a_provider_error() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(ProxyReply::Error(Fault::Internal));
    let failure = failure_of(&oagw).await;
    assert_eq!(failure.code, StreamErrorCode::ProviderError);
    assert_eq!(
        failure.message,
        StreamErrorCode::ProviderError.generic_message()
    );
    assert!(!failure.message.contains("boom"));
}

#[tokio::test]
async fn a_provider_429_is_rate_limited_with_the_retry_after_seconds() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(
        ProxyReply::json(429, &json!({ "error": { "message": "slow down" } }))
            .header("retry-after", "7")
            .from_upstream(),
    );
    let failure = failure_of(&oagw).await;
    assert_eq!(failure.code, StreamErrorCode::RateLimited);
    assert!(failure.message.contains("7 seconds"), "{}", failure.message);
}

#[tokio::test]
async fn the_gateways_504_is_a_provider_timeout() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(
        ProxyReply::json(
            504,
            &json!({ "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~", "status": 504 }),
        )
        .from_gateway(),
    );
    assert_eq!(
        failure_of(&oagw).await.code,
        StreamErrorCode::ProviderTimeout
    );
}

#[tokio::test]
async fn a_provider_error_body_is_relayed_sanitized() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(
        ProxyReply::json(
            400,
            &json!({ "error": { "code": "invalid_request", "message": "bad file file-abc123def456ghi" } }),
        )
        .from_upstream(),
    );
    let failure = failure_of(&oagw).await;
    assert_eq!(failure.code, StreamErrorCode::ProviderError);
    assert_eq!(failure.message, "bad file [provider_id]");
    assert_eq!(
        failure.detail.as_deref(),
        Some("invalid_request: bad file [provider_id]")
    );
}

#[tokio::test]
async fn a_successful_response_that_is_not_an_event_stream_is_a_provider_error() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(ProxyReply::json(200, &json!({ "id": "resp_1" })));
    let failure = failure_of(&oagw).await;
    assert_eq!(failure.code, StreamErrorCode::ProviderError);
    assert_eq!(
        failure.message,
        StreamErrorCode::ProviderError.generic_message()
    );
}

#[tokio::test]
async fn a_huge_error_body_is_read_boundedly() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(ProxyReply::Response {
        status: 502,
        headers: vec![("content-type".to_owned(), "text/html".to_owned())],
        body: crate::test_doubles::ReplyBody::Bytes(vec![b'x'; 4 << 20]),
        source: None,
    });
    let failure = failure_of(&oagw).await;
    assert_eq!(failure.code, StreamErrorCode::ProviderError);
}

#[tokio::test]
async fn a_failed_body_read_ends_the_stream_with_a_classified_failure() {
    let oagw = FakeOagw::new();
    let feed = oagw.push_gated();
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    feed.send_event(delta("a").0, &delta("a").1);
    feed.fail(Box::new(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "idle",
    )));
    assert_eq!(next(&mut stream).await, Some(Ok(text("a"))));
    let Some(Err(failure)) = next(&mut stream).await else {
        panic!("a failure expected");
    };
    assert_eq!(failure.code, StreamErrorCode::ProviderTimeout);
    assert_eq!(next(&mut stream).await, None, "nothing follows a failure");
}

#[tokio::test]
async fn another_body_error_is_a_provider_error() {
    let oagw = FakeOagw::new();
    let feed = oagw.push_gated();
    let client = OagwProviderClient::new(oagw.clone());
    let mut stream = open(&client).await.unwrap();
    feed.fail(Box::new(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "reset",
    )));
    let Some(Err(failure)) = next(&mut stream).await else {
        panic!("a failure expected");
    };
    assert_eq!(failure.code, StreamErrorCode::ProviderError);
}

#[tokio::test]
async fn a_pending_call_is_dropped_with_its_future() {
    let oagw = FakeOagw::new();
    oagw.push_proxy(ProxyReply::Pending);
    let client = OagwProviderClient::new(oagw.clone());
    let (ctx, request) = (user_ctx(), built());
    let attempt = tokio::time::timeout(Duration::from_millis(50), client.open(&ctx, &request));
    assert!(attempt.await.is_err(), "the call never answers");
    assert_eq!(
        oagw.pending_calls_dropped(),
        1,
        "the call future was dropped with the wait"
    );
}

// ---- the adapter seam ---------------------------------------------------------------------------

struct EchoAdapter(ProviderKind);

struct EchoTranslator;

impl EventTranslator for EchoTranslator {
    fn translate(&mut self, event: &SseEvent) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta {
            text: format!("echo:{}", event.data),
            item_id: None,
            content_index: None,
        }]
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        vec![ProviderEvent::Completed {
            usage: None,
            response_id: None,
            incomplete_reason: None,
        }]
    }
}

impl ProviderAdapter for EchoAdapter {
    fn kind(&self) -> ProviderKind {
        self.0
    }

    fn build_request(&self, input: &ProviderRequestInput<'_>) -> Result<BuiltRequest, DomainError> {
        Ok(BuiltRequest {
            kind: self.0,
            uri: format!("/{}/echo", input.target.alias),
            headers: Vec::new(),
            body: json!({ "echo": true }),
        })
    }

    fn translator(&self) -> Box<dyn EventTranslator> {
        Box::new(EchoTranslator)
    }
}

fn input<'a>(
    target: &'a ProviderTarget,
    entry: &'a mini_chat_sdk::ModelCatalogEntry,
    plan: &'a ContextPlan,
    tools: &'a ToolsDecision,
) -> ProviderRequestInput<'a> {
    ProviderRequestInput {
        target,
        entry,
        plan,
        tools,
        max_output_tokens_applied: 10,
        tenant_id: uuid::Uuid::from_u128(1),
        user_id: uuid::Uuid::from_u128(2),
        chat_id: uuid::Uuid::from_u128(3),
    }
}

#[test]
fn a_kind_without_an_adapter_cannot_build_a_request() {
    let client = OagwProviderClient::with_adapters(FakeOagw::new(), ProviderAdapters::empty());
    let target = ProviderTarget {
        kind: ProviderKind::OpenaiResponses,
        alias: "a".to_owned(),
        api_path: "/v1/responses".to_owned(),
        api_version: None,
    };
    let entry =
        crate::domain::service::quota::test_support::model("p", mini_chat_sdk::ModelTier::Premium)
            .build();
    let plan = ContextPlan {
        current: Some(CurrentMessage {
            content: "x".to_owned(),
            images: Vec::new(),
        }),
        ..ContextPlan::default()
    };
    let tools = ToolsDecision::default();
    let error = client
        .build_request(&input(&target, &entry, &plan, &tools))
        .unwrap_err();
    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
}

#[tokio::test]
async fn a_registered_adapter_builds_the_request_and_translates_the_response() {
    let oagw = FakeOagw::new();
    let mut adapters = ProviderAdapters::standard();
    adapters.register(std::sync::Arc::new(EchoAdapter(
        ProviderKind::VllmResponses,
    )));
    let client = OagwProviderClient::with_adapters(oagw.clone(), adapters);

    let target = ProviderTarget {
        kind: ProviderKind::VllmResponses,
        alias: "vllm.test".to_owned(),
        api_path: "/v1/responses".to_owned(),
        api_version: None,
    };
    let entry =
        crate::domain::service::quota::test_support::model("p", mini_chat_sdk::ModelTier::Premium)
            .build();
    let plan = ContextPlan::default();
    let tools = ToolsDecision::default();
    let request = client
        .build_request(&input(&target, &entry, &plan, &tools))
        .unwrap();
    assert_eq!(request.kind, ProviderKind::VllmResponses);
    assert_eq!(request.uri, "/vllm.test/echo");

    oagw.push_proxy(ProxyReply::sse_text(
        "data: one\n\ndata: two\n\n".to_owned(),
    ));
    let mut stream = within(client.open(&user_ctx(), &request)).await.unwrap();
    assert_eq!(next(&mut stream).await, Some(Ok(text("echo:one"))));
    assert_eq!(next(&mut stream).await, Some(Ok(text("echo:two"))));
    assert!(
        matches!(
            next(&mut stream).await,
            Some(Ok(ProviderEvent::Completed { .. }))
        ),
        "the translator's finish() is called at the end of the body"
    );
    assert_eq!(next(&mut stream).await, None);
}

#[test]
fn the_standard_adapters_serve_all_four_kinds() {
    let adapters = ProviderAdapters::standard();
    for kind in [
        ProviderKind::OpenaiResponses,
        ProviderKind::OpenaiChatCompletions,
        ProviderKind::VllmResponses,
        ProviderKind::AnthropicMessages,
    ] {
        let adapter = adapters
            .get(kind)
            .unwrap_or_else(|| panic!("no adapter for {kind:?}"));
        assert_eq!(adapter.kind(), kind);
    }
    assert!(
        ProviderAdapters::empty()
            .get(ProviderKind::OpenaiResponses)
            .is_none()
    );
}

#[test]
fn a_later_registration_of_the_same_kind_replaces_the_earlier_one() {
    let mut adapters = ProviderAdapters::standard();
    adapters.register(std::sync::Arc::new(EchoAdapter(
        ProviderKind::OpenaiResponses,
    )));
    let adapter = adapters.get(ProviderKind::OpenaiResponses).unwrap();
    let mut translator = adapter.translator();
    let events = translator.translate(&SseEvent {
        event: None,
        data: "x".to_owned(),
        id: None,
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], ProviderEvent::TextDelta { text, .. } if text == "echo:x"));
}

// ---- error logging does not leak secrets -----

#[tokio::test]
async fn error_response_body_containing_secrets_is_sanitized_in_logs() {
    let logs = LogCapture::start();
    let oagw = FakeOagw::new();
    let error_body = json!({
        "error": {
            "message": "Incorrect API key provided: sk-proj-abcdefghijklmnop. You can find your API key at https://platform.openai.com/account/api-keys.",
            "type": "invalid_request_error",
            "code": "invalid_api_key"
        }
    });
    oagw.push_proxy(ProxyReply::json(401, &error_body).from_upstream());
    let client = OagwProviderClient::new(oagw.clone());
    assert!(
        open(&client).await.is_err(),
        "expected error response with sensitive data"
    );

    let all_text = logs.all_text();
    assert!(
        !all_text.contains("sk-proj-abcdefghijklmnop"),
        "raw API key should not appear in logs"
    );
    assert!(
        !all_text.contains("req_abc123def456"),
        "request ID should not appear in logs"
    );
    assert!(
        !all_text.contains("platform.openai.com"),
        "URL should not appear in logs"
    );
}

#[tokio::test]
async fn call_error_before_response_is_sanitized_in_logs() {
    let logs = LogCapture::start();
    let oagw = FakeOagw::new();
    oagw.push_proxy(ProxyReply::Error(Fault::SecretNotReadable));
    let client = OagwProviderClient::new(oagw.clone());
    assert!(
        open(&client).await.is_err(),
        "expected gateway error before response"
    );

    // The error logs should not contain excessive details, only code and sanitized message
    let warn_logs = logs.at(Level::WARN);
    assert!(!warn_logs.is_empty(), "warning should be logged");
    let all_text = logs.all_text();
    // Check that logs contain our sanitized fields
    assert!(
        all_text.contains("uri_path=/api.openai.test/v1/responses"),
        "uri_path should be logged without query"
    );
    assert!(
        !all_text.contains("?api-version"),
        "query should not appear in logs"
    );
}

// ==== slice 7: the other adapters through the client =============================================

fn kind_input(
    kind: ProviderKind,
    api_path: &str,
) -> (
    ProviderTarget,
    mini_chat_sdk::ModelCatalogEntry,
    ContextPlan,
    ToolsDecision,
) {
    let target = ProviderTarget {
        kind,
        alias: "up.test".to_owned(),
        api_path: api_path.to_owned(),
        api_version: None,
    };
    let entry =
        crate::domain::service::quota::test_support::model("p", mini_chat_sdk::ModelTier::Premium)
            .build();
    let plan = ContextPlan {
        instructions: "SYS".to_owned(),
        current: Some(CurrentMessage {
            content: "hi".to_owned(),
            images: Vec::new(),
        }),
        ..ContextPlan::default()
    };
    (target, entry, plan, ToolsDecision::default())
}

fn build_for(client: &OagwProviderClient, kind: ProviderKind, api_path: &str) -> BuiltRequest {
    let (target, entry, plan, tools) = kind_input(kind, api_path);
    client
        .build_request(&input(&target, &entry, &plan, &tools))
        .expect("the request is built")
}

async fn drain(stream: &mut Box<dyn ProviderStream>) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    while let Some(event) = next(stream).await {
        out.push(event.expect("no failure"));
    }
    out
}

#[tokio::test]
async fn chat_completions_are_sent_and_translated_through_the_client() {
    let oagw = FakeOagw::new();
    let client = OagwProviderClient::new(oagw.clone());
    let request = build_for(
        &client,
        ProviderKind::OpenaiChatCompletions,
        "/v1/chat/completions",
    );
    assert_eq!(request.kind, ProviderKind::OpenaiChatCompletions);
    oagw.push_proxy(ProxyReply::sse_text(
        [
            r#"data: {"id":"chatcmpl-1","choices":[{"delta":{"content":"Hel"}}],"usage":null}"#,
            r#"data: {"id":"chatcmpl-1","choices":[{"delta":{"content":"lo"}}],"usage":null}"#,
            r#"data: {"id":"chatcmpl-1","choices":[{"delta":{},"finish_reason":"stop"}],"usage":null}"#,
            r#"data: {"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
            "data: [DONE]",
        ]
        .join("\n\n")
            + "\n\n",
    ));
    let mut stream = within(client.open(&user_ctx(), &request)).await.unwrap();
    let events = drain(&mut stream).await;
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0], text("Hel"));
    assert_eq!(events[1], text("lo"));
    let ProviderEvent::Completed {
        usage, response_id, ..
    } = &events[2]
    else {
        panic!("completion expected: {events:?}");
    };
    assert_eq!(usage.unwrap().input_tokens, 5);
    assert_eq!(response_id.as_deref(), Some("chatcmpl-1"));
    let sent = oagw.proxied();
    assert_eq!(sent[0].uri, "/up.test/v1/chat/completions");
    assert_eq!(
        sent[0].json()["stream_options"],
        json!({ "include_usage": true })
    );
}

#[tokio::test]
async fn a_chat_completions_stream_that_ends_without_done_still_releases_its_outcome() {
    let oagw = FakeOagw::new();
    let client = OagwProviderClient::new(oagw.clone());
    let request = build_for(
        &client,
        ProviderKind::OpenaiChatCompletions,
        "/v1/chat/completions",
    );
    oagw.push_proxy(ProxyReply::sse_text(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ));
    let mut stream = within(client.open(&user_ctx(), &request)).await.unwrap();
    let events = drain(&mut stream).await;
    assert!(
        matches!(
            events.as_slice(),
            [ProviderEvent::Completed { usage: None, .. }]
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn an_anthropic_request_carries_the_version_header_and_its_stream_is_translated() {
    let oagw = FakeOagw::new();
    let client = OagwProviderClient::new(oagw.clone());
    let request = build_for(&client, ProviderKind::AnthropicMessages, "/v1/messages");
    assert_eq!(request.kind, ProviderKind::AnthropicMessages);
    oagw.push_proxy(ProxyReply::sse(&[
        (
            "message_start",
            json!({ "type": "message_start", "message": { "id": "msg_1",
                    "usage": { "input_tokens": 2, "cache_read_input_tokens": 3, "output_tokens": 1 } } }),
        ),
        (
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "Hi" } }),
        ),
        (
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 4 } }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ]));
    let mut stream = within(client.open(&user_ctx(), &request)).await.unwrap();
    let events = drain(&mut stream).await;
    assert_eq!(events.len(), 2, "{events:?}");
    let ProviderEvent::Completed { usage, .. } = &events[1] else {
        panic!("completion expected: {events:?}");
    };
    let usage = usage.unwrap();
    assert_eq!((usage.input_tokens, usage.cache_read_input_tokens), (5, 3));
    let sent = oagw.proxied();
    assert_eq!(sent[0].uri, "/up.test/v1/messages");
    assert_eq!(sent[0].header("anthropic-version"), Some("2023-06-01"));
    assert!(sent[0].header("x-api-key").is_none());
    assert_eq!(sent[0].json()["max_tokens"], 10);
}

#[tokio::test]
async fn a_vllm_stream_splits_think_text_into_reasoning_deltas() {
    let oagw = FakeOagw::new();
    let client = OagwProviderClient::new(oagw.clone());
    let request = build_for(&client, ProviderKind::VllmResponses, "/v1/responses");
    assert_eq!(request.kind, ProviderKind::VllmResponses);
    oagw.push_proxy(ProxyReply::sse(&[
        (
            "response.output_text.delta",
            json!({ "type": "response.output_text.delta", "delta": "<think>why</think>because" }),
        ),
        ("response.completed", json!({ "response": { "id": "resp_1", "usage": { "input_tokens": 1, "output_tokens": 2 } } })),
    ]));
    let mut stream = within(client.open(&user_ctx(), &request)).await.unwrap();
    let events = drain(&mut stream).await;
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(
        events[0],
        ProviderEvent::ReasoningDelta {
            text: "why".to_owned()
        }
    );
    assert_eq!(events[1], text("because"));
    assert!(matches!(events[2], ProviderEvent::Completed { .. }));
}

#[test]
fn the_continuation_is_built_by_the_adapter_of_the_previous_request() {
    let client = OagwProviderClient::new(FakeOagw::new());
    let outputs = vec![crate::domain::service::stream::ports::ToolCallOutput {
        call: crate::domain::service::stream::types::FunctionCall {
            call_id: "c1".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: "{\"query\":\"q\"}".to_owned(),
        },
        output: "{\"results\":[]}".to_owned(),
    }];
    for (kind, path, key) in [
        (ProviderKind::OpenaiResponses, "/v1/responses", "input"),
        (ProviderKind::VllmResponses, "/v1/responses", "input"),
        (
            ProviderKind::OpenaiChatCompletions,
            "/v1/chat/completions",
            "messages",
        ),
        (ProviderKind::AnthropicMessages, "/v1/messages", "messages"),
    ] {
        let previous = build_for(&client, kind, path);
        let before = previous.body[key].as_array().unwrap().len();
        let next = client.continue_request(&previous, &outputs).unwrap();
        assert_eq!(next.kind, kind);
        assert_eq!(next.uri, previous.uri);
        assert!(
            next.body[key].as_array().unwrap().len() > before,
            "{kind:?} appended nothing"
        );
    }
}

#[test]
fn a_continuation_for_a_kind_without_an_adapter_is_an_internal_error() {
    let client = OagwProviderClient::with_adapters(FakeOagw::new(), ProviderAdapters::empty());
    let error = client.continue_request(&built(), &[]).unwrap_err();
    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
}
