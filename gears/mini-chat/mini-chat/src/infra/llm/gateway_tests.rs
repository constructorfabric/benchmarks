use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mini_chat_sdk::{ApiParams, UsageTokens};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::*;
use crate::config::ProviderKind;
use crate::infra::llm::sanitize::provider_user_field;
use crate::infra::llm::types::{InputMessage, LlmTerminal, RequestMetadata, Role};
use crate::test_support::FakeOagw;
use crate::test_support::fake_oagw::gateway_timeout_error;

const PATH: &str = "/v1/responses";

fn target() -> ProviderTarget {
    ProviderTarget {
        provider_id: "openai".into(),
        kind: ProviderKind::OpenaiResponses,
        alias: "api.openai.com".into(),
        api_path: PATH.into(),
    }
}

fn request(stream: bool) -> LlmRequest {
    let (t, u, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
    LlmRequest {
        model: "gpt-x".into(),
        instructions: "SYS".into(),
        input: vec![InputMessage::text(Role::User, "hi")],
        max_output_tokens: 100,
        tools: Vec::new(),
        max_tool_calls: 2,
        api_params: ApiParams {
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: Vec::new(),
            extra_body: None,
            reasoning_effort: None,
        },
        user: provider_user_field(t, u),
        metadata: RequestMetadata::chat(t, u, c, &[]),
        stream,
    }
}

fn gateway() -> (Arc<FakeOagw>, LlmGateway) {
    let fake = Arc::new(FakeOagw::new());
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::from_u128(10))
        .subject_tenant_id(Uuid::from_u128(11))
        .build()
        .unwrap();
    let gw = LlmGateway::new(fake.clone(), Arc::new(S2sContextProvider::fixed(ctx)));
    (fake, gw)
}

fn delta(text: &str) -> (&'static str, serde_json::Value) {
    (
        "response.output_text.delta",
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": text}),
    )
}

fn completed() -> (&'static str, serde_json::Value) {
    (
        "response.completed",
        json!({"type": "response.completed", "response": {
            "id": "resp_1", "usage": {"input_tokens": 5, "output_tokens": 2},
        }}),
    )
}

async fn stream_err(gw: &LlmGateway) -> ProviderFailure {
    match gw
        .stream(&target(), request(true), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("expected a provider failure"),
        Err(f) => f,
    }
}

#[tokio::test]
async fn stream_posts_adapter_body_and_yields_events() {
    let (fake, gw) = gateway();
    fake.push_sse(PATH, vec![delta("Hel"), delta("lo"), completed()]);
    let events: Vec<LlmEvent> = gw
        .stream(&target(), request(true), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("Hel".into()),
            LlmEvent::TextDelta("lo".into()),
            LlmEvent::Completed(LlmTerminal {
                usage: Some(UsageTokens {
                    input_tokens: 5,
                    output_tokens: 2,
                    ..UsageTokens::default()
                }),
                response_id: Some("resp_1".into()),
            }),
        ]
    );
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].uri, "/api.openai.com/v1/responses");
    let body = reqs[0].json_body.as_ref().unwrap();
    assert_eq!(body["model"], "gpt-x");
    assert_eq!(body["stream"], true);
    assert_eq!(fake.open_streams(), 0);
}

#[tokio::test]
async fn http_429_maps_rate_limited_with_retry_after() {
    let (fake, gw) = gateway();
    fake.push_json_with_headers(
        PATH,
        429,
        &[("retry-after", "7")],
        json!({"error": {"message": "Rate limit reached for org-abc"}}),
    );
    let f = stream_err(&gw).await;
    assert_eq!(f.code, StreamErrorCode::RateLimited);
    assert!(f.message.contains("retry in 7s"), "{}", f.message);

    // Without a numeric Retry-After there is no delay in the message.
    fake.push_json(PATH, 429, json!({"error": {"message": "slow down"}}));
    let f = stream_err(&gw).await;
    assert_eq!(f.code, StreamErrorCode::RateLimited);
    assert!(!f.message.contains("retry in"), "{}", f.message);
}

#[tokio::test]
async fn http_500_json_error_is_provider_error_sanitized() {
    let (fake, gw) = gateway();
    fake.push_json(
        PATH,
        500,
        json!({"error": {"message": "File file-abc123def456ghi not found (request resp_123abc)", "type": "server_error"}}),
    );
    let f = stream_err(&gw).await;
    assert_eq!(f.code, StreamErrorCode::ProviderError);
    assert!(f.message.contains("[provider_id]"), "{}", f.message);
    assert!(!f.message.contains("file-abc123def456ghi"), "{}", f.message);
    assert!(!f.message.contains("resp_123abc"), "{}", f.message);
    assert_eq!(f.usage, None);
}

#[tokio::test]
async fn non_json_error_body_gets_generic_message() {
    let (fake, gw) = gateway();
    fake.push_json(PATH, 502, json!("<html>bad gateway</html>"));
    let f = stream_err(&gw).await;
    assert_eq!(f.code, StreamErrorCode::ProviderError);
    assert!(!f.message.contains("html"), "{}", f.message);
}

#[tokio::test]
async fn oagw_504_is_provider_timeout() {
    let (fake, gw) = gateway();
    // Gateway timeout reported as an error by the in-process proxy.
    fake.push_error(PATH, gateway_timeout_error());
    assert_eq!(stream_err(&gw).await.code, StreamErrorCode::ProviderTimeout);

    // The gateway's own HTTP 504 `deadline_exceeded` Problem.
    fake.push_gateway_response(
        PATH,
        504,
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~",
            "title": "Deadline Exceeded",
            "status": 504,
            "detail": "upstream request timed out",
        }),
    );
    assert_eq!(stream_err(&gw).await.code, StreamErrorCode::ProviderTimeout);

    // A provider's own 504 with its JSON error body is a provider error.
    fake.push_json(PATH, 504, json!({"error": {"message": "upstream timeout"}}));
    assert_eq!(stream_err(&gw).await.code, StreamErrorCode::ProviderError);
}

#[tokio::test]
async fn stream_yields_deltas_before_completion() {
    let (fake, gw) = gateway();
    let delay = Duration::from_millis(400);
    fake.push_sse_slow(
        PATH,
        vec![delta("first"), delta("second"), completed()],
        delay,
    );
    let started = Instant::now();
    let mut s = gw
        .stream(&target(), request(true), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(s.next().await, Some(LlmEvent::TextDelta("first".into())));
    // The first delta arrives while the provider is still streaming.
    assert!(started.elapsed() < delay, "first delta was buffered");
    assert_eq!(fake.open_streams(), 1);
    let rest: Vec<LlmEvent> = s.collect().await;
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0], LlmEvent::TextDelta("second".into()));
    assert!(matches!(rest[1], LlmEvent::Completed(_)));
}

#[tokio::test]
async fn cancel_drops_stream() {
    let (fake, gw) = gateway();
    fake.push_sse_slow(
        PATH,
        vec![delta("first"), delta("never")],
        Duration::from_secs(30),
    );
    let cancel = CancellationToken::new();
    let mut s = gw
        .stream(&target(), request(true), cancel.clone())
        .await
        .unwrap();
    assert_eq!(s.next().await, Some(LlmEvent::TextDelta("first".into())));
    assert_eq!(fake.open_streams(), 1);
    cancel.cancel();
    let next = tokio::time::timeout(Duration::from_secs(2), s.next())
        .await
        .expect("stream ends promptly after cancel");
    assert_eq!(next, None);
    assert_eq!(fake.open_streams(), 0, "provider body must be dropped");
}

#[tokio::test]
async fn stream_without_terminal_event_fails() {
    let (fake, gw) = gateway();
    fake.push_sse(PATH, vec![delta("partial")]);
    let events: Vec<LlmEvent> = gw
        .stream(&target(), request(true), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], LlmEvent::TextDelta("partial".into()));
    assert!(
        matches!(&events[1], LlmEvent::Failed(f) if f.code == StreamErrorCode::ProviderError),
        "{events:?}"
    );
}

#[tokio::test]
async fn stream_stops_after_terminal_event() {
    let (fake, gw) = gateway();
    fake.push_sse(PATH, vec![completed(), delta("after")]);
    let events: Vec<LlmEvent> = gw
        .stream(&target(), request(true), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], LlmEvent::Completed(_)));
}

#[tokio::test]
async fn complete_parses_non_streaming_body() {
    let (fake, gw) = gateway();
    fake.push_json(
        PATH,
        200,
        json!({
            "id": "resp_s",
            "status": "completed",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "summary"}]}],
            "usage": {"input_tokens": 9, "output_tokens": 1},
        }),
    );
    let c = gw.complete(&target(), request(false)).await.unwrap();
    assert_eq!(c.text, "summary");
    assert_eq!(c.response_id.as_deref(), Some("resp_s"));
    assert_eq!(
        fake.requests()[0].json_body.as_ref().unwrap()["stream"],
        false
    );
}

#[tokio::test]
async fn complete_maps_http_errors() {
    let (fake, gw) = gateway();
    fake.push_json_with_headers(PATH, 429, &[("retry-after", "3")], json!({}));
    let f = gw.complete(&target(), request(false)).await.unwrap_err();
    assert_eq!(f.code, StreamErrorCode::RateLimited);
    assert!(f.message.contains("retry in 3s"));

    fake.push_error(PATH, gateway_timeout_error());
    let f = gw.complete(&target(), request(false)).await.unwrap_err();
    assert_eq!(f.code, StreamErrorCode::ProviderTimeout);
}

#[tokio::test]
async fn anthropic_target_uses_messages_adapter_and_version_header() {
    let (fake, gw) = gateway();
    let t = ProviderTarget {
        provider_id: "anthropic".into(),
        kind: ProviderKind::AnthropicMessages,
        alias: "api.anthropic.com".into(),
        api_path: "/v1/messages".into(),
    };
    fake.push_sse(
        "/v1/messages",
        vec![
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 4, "output_tokens": 1}}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ],
    );
    let events: Vec<LlmEvent> = gw
        .stream(&t, request(true), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(events[0], LlmEvent::TextDelta("Hi".into()));
    assert!(matches!(events[1], LlmEvent::Completed(_)));
    let reqs = fake.requests();
    assert_eq!(reqs[0].uri, "/api.anthropic.com/v1/messages");
    assert_eq!(reqs[0].json_body.as_ref().unwrap()["max_tokens"], 100);
    assert!(
        reqs[0]
            .headers
            .contains(&("anthropic-version".to_owned(), "2023-06-01".to_owned())),
        "{:?}",
        reqs[0].headers
    );
}
