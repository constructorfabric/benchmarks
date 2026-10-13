#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use mini_chat_sdk::ModelApiParams;
use oagw_sdk::ServiceGatewayError;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use uuid::Uuid;

use super::{LlmClient, gateway_error};
use crate::config::MiniChatConfig;
use crate::domain::model::MessageRole;
use crate::infra::llm::{
    LlmError, LlmEvent, LlmMessage, LlmRequest, LlmUsage, ProviderResolver, RequestMetadata,
    RequestType, ResolvedProvider,
};
use crate::infra::oagw::s2s::S2sContext;
use crate::testing::{FakeProvider, ScriptedStream, TestUser};

fn provider() -> ResolvedProvider {
    let mut cfg = MiniChatConfig::default();
    cfg.apply_defaults();
    ProviderResolver::new(&cfg)
        .resolve("openai", TestUser::A1.tenant_id)
        .unwrap()
}

fn request(stream: bool) -> LlmRequest {
    LlmRequest {
        model: "gpt-5.2".to_owned(),
        instructions: "sys".to_owned(),
        input: vec![LlmMessage::text(MessageRole::User, "hi")],
        max_output_tokens: 100,
        tools: Vec::new(),
        max_tool_calls: None,
        api_params: ModelApiParams::default(),
        user: "u".to_owned(),
        metadata: RequestMetadata::new(
            TestUser::A1.tenant_id,
            TestUser::A1.user_id,
            Uuid::nil(),
            RequestType::Chat,
            &[],
        ),
        stream,
        tool_rounds: Vec::new(),
    }
}

fn client() -> (LlmClient, Arc<FakeProvider>) {
    let fake = FakeProvider::new();
    let s2s = Arc::new(S2sContext::new());
    s2s.set(TestUser::S2S.security_context());
    (LlmClient::new(fake.clone(), s2s), fake)
}

async fn collect(client: &LlmClient) -> Result<Vec<LlmEvent>, LlmError> {
    let s = client
        .stream(&provider(), &request(true), CancellationToken::new())
        .await?;
    Ok(s.collect().await)
}

#[tokio::test]
async fn streams_text_and_completed() {
    let (c, fake) = client();
    fake.push_stream(ScriptedStream::text(&["Hel", "lo"], 10, 5));
    let events = collect(&c).await.unwrap();
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("Hel".to_owned()),
            LlmEvent::TextDelta("lo".to_owned()),
            LlmEvent::Completed {
                usage: Some(LlmUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..LlmUsage::default()
                }),
                response_id: Some("resp_fake0001".to_owned()),
                incomplete_reason: None,
            },
        ]
    );
}

#[tokio::test]
async fn requests_use_s2s_context_and_alias_path() {
    let (c, fake) = client();
    collect(&c).await.unwrap();
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, http::Method::POST);
    assert_eq!(reqs[0].path, "/api.openai.com/v1/responses");
    assert_eq!(reqs[0].query, None);
    assert_eq!(reqs[0].content_type.as_deref(), Some("application/json"));
    assert_eq!(reqs[0].subject_tenant_id, TestUser::S2S.tenant_id);
    assert_eq!(reqs[0].subject_id, TestUser::S2S.user_id);
    assert_eq!(fake.chat_requests()[0]["model"], json!("gpt-5.2"));
}

#[tokio::test]
async fn missing_s2s_context_fails_before_sending() {
    let fake = FakeProvider::new();
    let c = LlmClient::new(fake.clone(), Arc::new(S2sContext::new()));
    let err = collect(&c).await.unwrap_err();
    assert_eq!(err.sse_code(), "provider_error");
    assert_eq!(err.client_message(), "Provider is currently unavailable");
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn http_429_maps_to_rate_limited_with_retry_after() {
    let (c, fake) = client();
    fake.push_stream(ScriptedStream::rate_limited(7));
    let err = collect(&c).await.unwrap_err();
    assert_eq!(
        err,
        LlmError::RateLimited {
            retry_after_secs: Some(7),
            message: "Rate limit reached".to_owned(),
        }
    );
    assert_eq!(err.sse_code(), "rate_limited");
    assert!(
        err.client_message().contains("retry in 7s"),
        "{}",
        err.client_message()
    );
}

#[tokio::test]
async fn gateway_504_maps_to_timeout() {
    let (c, fake) = client();
    fake.push_stream(ScriptedStream::gateway_timeout());
    let err = collect(&c).await.unwrap_err();
    assert!(matches!(err, LlmError::Timeout(_)), "{err:?}");
    assert_eq!(err.sse_code(), "provider_timeout");
}

#[tokio::test]
async fn upstream_504_is_provider_error() {
    let (c, fake) = client();
    fake.push_stream(ScriptedStream::http(
        504,
        json!({"error": {"message": "upstream gateway timeout"}}),
    ));
    let err = collect(&c).await.unwrap_err();
    assert_eq!(
        err,
        LlmError::Provider {
            message: "upstream gateway timeout".to_owned()
        }
    );
}

#[tokio::test]
async fn http_500_body_message_sanitized() {
    let (c, fake) = client();
    fake.push_stream(ScriptedStream::http(
        500,
        json!({"error": {"message": "bad file-abcdefghijklmnop in resp_123 see https://x.y/z key sk-abcdefghijkl"}}),
    ));
    let err = collect(&c).await.unwrap_err();
    assert_eq!(err.sse_code(), "provider_error");
    let msg = err.client_message();
    for leak in ["file-abc", "resp_123", "https://", "sk-abc"] {
        assert!(!msg.contains(leak), "{msg}");
    }
    assert!(msg.starts_with("bad [provider_id]"), "{msg}");

    // Without a JSON error body the status is reported.
    fake.push_stream(ScriptedStream::http(502, json!("oops")));
    let err = collect(&c).await.unwrap_err();
    assert!(err.client_message().contains("502"), "{err:?}");
}

#[test]
fn proxy_errors_map_through_service_gateway_error() {
    assert!(matches!(
        gateway_error(&ServiceGatewayError::Timeout),
        LlmError::Timeout(_)
    ));
    assert!(matches!(
        gateway_error(&ServiceGatewayError::RateLimited {
            retry_after_secs: Some(3)
        }),
        LlmError::RateLimited {
            retry_after_secs: Some(3),
            ..
        }
    ));
    // Gateway internals never reach the client message.
    let internal = ServiceGatewayError::from(
        CanonicalError::internal("pingora exploded at https://x.y").create(),
    );
    assert_eq!(
        gateway_error(&internal).client_message(),
        "Provider is currently unavailable"
    );
    let unavailable = ServiceGatewayError::from(CanonicalError::service_unavailable().create());
    assert!(matches!(
        gateway_error(&unavailable),
        LlmError::Unavailable(_)
    ));
    assert_eq!(gateway_error(&unavailable).sse_code(), "provider_error");
}

#[tokio::test]
async fn failed_event_is_terminal() {
    let (c, fake) = client();
    fake.push_stream(ScriptedStream::failed("bad resp_123"));
    let events = collect(&c).await.unwrap();
    assert_eq!(
        events,
        vec![LlmEvent::Failed {
            error: LlmError::Provider {
                message: "bad resp_123".to_owned()
            },
            usage: None,
        }]
    );
}

#[tokio::test]
async fn stream_end_without_terminal_is_provider_error() {
    let (c, fake) = client();
    fake.push_stream(ScriptedStream::no_terminal(&["par", "tial"]));
    let events = collect(&c).await.unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0], LlmEvent::TextDelta("par".to_owned()));
    assert_eq!(events[1], LlmEvent::TextDelta("tial".to_owned()));
    let LlmEvent::Failed { error, usage } = &events[2] else {
        panic!("expected Failed, got {:?}", events[2]);
    };
    assert_eq!(error.sse_code(), "provider_error");
    assert_eq!(*usage, None);
}

#[tokio::test]
async fn events_after_terminal_are_dropped() {
    let (c, fake) = client();
    let mut s = ScriptedStream::text(&["a"], 1, 1);
    s.events
        .push(crate::testing::fake_provider::delta_event("late"));
    fake.push_stream(s);
    let events = collect(&c).await.unwrap();
    assert_eq!(events.len(), 2);
    assert!(events[1].is_terminal());
}

#[tokio::test]
async fn first_delta_arrives_before_stream_finishes() {
    let (c, fake) = client();
    fake.push_stream(ScriptedStream {
        hold_after: Some(2),
        ..ScriptedStream::text(&["first", "second"], 1, 1)
    });
    let mut s = c
        .stream(&provider(), &request(true), CancellationToken::new())
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), s.next())
        .await
        .expect("first delta not buffered");
    assert_eq!(first, Some(LlmEvent::TextDelta("first".to_owned())));
    fake.release();
    let rest: Vec<_> = s.collect().await;
    assert_eq!(rest.len(), 2);
}

#[tokio::test]
async fn cancel_token_stops_stream() {
    let (c, fake) = client();
    fake.push_stream(ScriptedStream {
        hold_after: Some(2),
        ..ScriptedStream::text(&["first", "second"], 1, 1)
    });
    let cancel = CancellationToken::new();
    let mut s = c
        .stream(&provider(), &request(true), cancel.clone())
        .await
        .unwrap();
    assert_eq!(
        s.next().await,
        Some(LlmEvent::TextDelta("first".to_owned()))
    );
    assert_eq!(fake.open_streams(), 1);

    cancel.cancel();
    let next = tokio::time::timeout(Duration::from_secs(2), s.next())
        .await
        .expect("cancel must end the stream promptly");
    assert_eq!(next, None);
    assert_eq!(fake.open_streams(), 0, "provider body must be dropped");
}

#[tokio::test]
async fn complete_parses_non_streaming_response() {
    let (c, fake) = client();
    fake.push_completion(
        "<summary>ok</summary>",
        Some(LlmUsage {
            input_tokens: 20,
            output_tokens: 8,
            reasoning_tokens: 2,
            ..LlmUsage::default()
        }),
    );
    let r = c.complete(&provider(), &request(false)).await.unwrap();
    assert_eq!(r.text, "<summary>ok</summary>");
    assert_eq!(r.usage.unwrap().reasoning_tokens, 2);
    assert_eq!(fake.chat_requests()[0]["stream"], json!(false));

    fake.fail_next("/v1/responses", 500);
    let err = c.complete(&provider(), &request(false)).await.unwrap_err();
    assert_eq!(err.sse_code(), "provider_error");
}
