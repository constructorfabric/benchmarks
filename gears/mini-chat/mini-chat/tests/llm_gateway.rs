//! The services built by the harness send provider calls through
//! `TestApp.oagw` (the fake OAGW) using the provider resolver.

mod common;

use futures::StreamExt;
use mini_chat::infra::llm::sanitize::provider_user_field;
use mini_chat::infra::llm::types::{InputMessage, LlmEvent, LlmRequest, RequestMetadata, Role};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use common::*;

fn request(tenant: Uuid, user: Uuid) -> LlmRequest {
    let catalog = standard_model("s1");
    LlmRequest {
        model: catalog.provider_model_id,
        instructions: catalog.system_prompt,
        input: vec![InputMessage::text(Role::User, "hello")],
        max_output_tokens: catalog.max_output_tokens,
        tools: Vec::new(),
        max_tool_calls: catalog.max_tool_calls,
        api_params: catalog.general_config.api_params,
        user: provider_user_field(tenant, user),
        metadata: RequestMetadata::chat(tenant, user, Uuid::new_v4(), &[]),
        stream: true,
    }
}

#[tokio::test]
async fn services_stream_through_the_harness_oagw() {
    let app = TestApp::builder().build().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    app.oagw.push_sse(
        "/v1/responses",
        vec![
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "delta": "Hi"}),
            ),
            (
                "response.completed",
                json!({"type": "response.completed", "response": {"id": "resp_1"}}),
            ),
        ],
    );

    let target = app.services.providers.resolve("openai", tenant).unwrap();
    let events: Vec<LlmEvent> = app
        .services
        .llm
        .stream(&target, request(tenant, user), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;

    assert_eq!(events[0], LlmEvent::TextDelta("Hi".into()));
    assert!(matches!(events[1], LlmEvent::Completed(_)));
    let reqs = app.oagw.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].uri, "/api.openai.com/v1/responses");
    let body = reqs[0].json_body.as_ref().unwrap();
    assert_eq!(body["model"], "s1-provider-model");
    assert_eq!(body["user"], provider_user_field(tenant, user));
}

#[tokio::test]
async fn services_storage_reaches_the_harness_oagw() {
    let app = TestApp::builder().build().await;
    app.oagw
        .push_json("/v1/files", 200, json!({"id": "file-harness-1"}));

    let target = app
        .services
        .providers
        .resolve_storage("openai", Uuid::new_v4())
        .unwrap();
    let id = app
        .services
        .storage
        .upload_file(&target, "a.txt", "text/plain", bytes::Bytes::from("hi"))
        .await
        .unwrap();

    assert_eq!(id, "file-harness-1");
    assert_eq!(app.oagw.requests()[0].uri, "/api.openai.com/v1/files");
}
