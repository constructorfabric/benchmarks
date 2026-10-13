//! The harness wires the LLM client to the fake provider with the S2S context.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use mini_chat::domain::model::MessageRole;
use mini_chat::infra::llm::{LlmEvent, LlmMessage, LlmRequest, RequestMetadata, RequestType};
use mini_chat::testing::{ScriptedStream, TestApp, TestUser};
use mini_chat_sdk::ModelApiParams;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[tokio::test]
async fn services_stream_through_fake_provider_with_s2s_identity() {
    let app = TestApp::builder().build().await;
    app.provider
        .push_stream(ScriptedStream::text(&["Hi"], 3, 1));

    let provider = app
        .services
        .providers
        .resolve("openai", TestUser::A1.tenant_id)
        .unwrap();
    let req = LlmRequest {
        model: "gpt-premium".to_owned(),
        instructions: String::new(),
        input: vec![LlmMessage::text(MessageRole::User, "hello")],
        max_output_tokens: 16,
        tools: Vec::new(),
        max_tool_calls: None,
        api_params: ModelApiParams::default(),
        user: "u".to_owned(),
        metadata: RequestMetadata::new(
            TestUser::A1.tenant_id,
            TestUser::A1.user_id,
            Uuid::new_v4(),
            RequestType::Chat,
            &[],
        ),
        stream: true,
        tool_rounds: Vec::new(),
    };
    let events: Vec<LlmEvent> = app
        .services
        .llm
        .stream(&provider, &req, CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(events[0], LlmEvent::TextDelta("Hi".to_owned()));
    assert!(events[1].is_terminal());

    let recorded = app.provider.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].path, "/api.openai.com/v1/responses");
    assert_eq!(recorded[0].subject_tenant_id, TestUser::S2S.tenant_id);
    assert_eq!(recorded[0].subject_id, TestUser::S2S.user_id);
    assert_eq!(
        app.services.s2s.get().unwrap().subject_id(),
        TestUser::S2S.user_id
    );
}
