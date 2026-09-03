//! Pattern: Custom base URL (over HTTP)
//!
//! Uses `async-openai`'s native reqwest-based client with `api_base` pointed
//! at an axum mock server that mimics OAGW's proxy endpoint. This is the
//! zero-code integration — just a URL change.
//!
//! Note: the `api_key` here is only needed to satisfy the SDK's config
//! requirements. In production, OAGW handles upstream authentication
//! transparently — the caller's key is not forwarded to the third-party API.

use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, CreateChatCompletionRequestArgs,
};
use tokio::net::TcpListener;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// Canned OpenAI response
// ---------------------------------------------------------------------------

const CANNED_CHAT_RESPONSE: &str = r#"{
    "id": "chatcmpl-test-456",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "gpt-4o",
    "choices": [{
        "index": 0,
        "message": {
            "role": "assistant",
            "content": "Hello from OAGW via HTTP!"
        },
        "finish_reason": "stop"
    }],
    "usage": {
        "prompt_tokens": 10,
        "completion_tokens": 6,
        "total_tokens": 16
    }
}"#;

// ---------------------------------------------------------------------------
// Mock server mimicking OAGW's proxy endpoint
// ---------------------------------------------------------------------------

async fn mock_openai_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    let app = axum::Router::new().route(
        "/oagw/v1/proxy/openai/v1/chat/completions",
        axum::routing::post(|| async {
            axum::response::Json(
                serde_json::from_str::<serde_json::Value>(CANNED_CHAT_RESPONSE).unwrap(),
            )
        }),
    );

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (base_url, handle)
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn custom_base_url_chat_completion() -> TestResult {
    // -- setup: start mock server -----------------------------------------------
    let (base_url, server_handle) = mock_openai_server().await;

    // Point async-openai at OAGW's proxy endpoint instead of api.openai.com
    let config = OpenAIConfig::new()
        .with_api_base(format!("{base_url}/oagw/v1/proxy/openai/v1"))
        .with_api_key("test-key");

    let client = Client::with_config(config);

    // -- action: use the native async-openai client ----------------------------
    let request = CreateChatCompletionRequestArgs::default()
        .model("gpt-4o")
        .messages(vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text("Say hello".into()),
                name: None,
            },
        )])
        .build()?;

    let response = client.chat().create(request).await?;

    // -- verify: response deserialized by async-openai --------------------------
    assert_eq!(response.id, "chatcmpl-test-456");
    assert_eq!(response.model, "gpt-4o");
    assert_eq!(response.choices.len(), 1);
    assert_eq!(
        response.choices[0].message.content.as_deref(),
        Some("Hello from OAGW via HTTP!")
    );

    server_handle.abort();
    Ok(())
}
