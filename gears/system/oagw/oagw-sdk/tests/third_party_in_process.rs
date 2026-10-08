//! Pattern: In-process transport via `ServiceGatewayClientV1`
//!
//! Uses real third-party SDK types (`async-openai`) for request/response
//! serialization, but calls `proxy_request` directly as the HTTP transport.
//! No reqwest, no HTTP server — just a thin `GatewayTransport` adapter.
//!
//! Note: authentication (API keys, OAuth2, etc.) is configured on the OAGW
//! upstream and injected transparently by the gateway during the proxy
//! pipeline — callers never handle third-party credentials.

use std::sync::{Arc, Mutex};

use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, CreateChatCompletionRequestArgs,
    CreateChatCompletionResponse,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::body::{Body, BodyStream, BoxError};
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use serde::{Serialize, de::DeserializeOwned};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// Canned OpenAI response
// ---------------------------------------------------------------------------

const CANNED_CHAT_RESPONSE: &str = r#"{
    "id": "chatcmpl-test-123",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "gpt-4o",
    "choices": [{
        "index": 0,
        "message": {
            "role": "assistant",
            "content": "Hello from OAGW!"
        },
        "finish_reason": "stop"
    }],
    "usage": {
        "prompt_tokens": 10,
        "completion_tokens": 5,
        "total_tokens": 15
    }
}"#;

fn canned_response() -> http::Response<Body> {
    http::Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Body::from(CANNED_CHAT_RESPONSE))
        .unwrap()
}

/// Build an SSE response with a streaming body from the provided chunks.
fn server_events_response(chunks: Vec<&str>) -> http::Response<Body> {
    let owned: Vec<Result<Bytes, BoxError>> = chunks
        .into_iter()
        .map(|s| Ok(Bytes::from(s.to_owned())))
        .collect();
    let stream: BodyStream = Box::pin(futures_util::stream::iter(owned));
    http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::Stream(stream))
        .unwrap()
}

// ---------------------------------------------------------------------------
// MockGateway — same pattern as usage.rs
// ---------------------------------------------------------------------------

struct MockGateway {
    response: Mutex<Option<http::Response<Body>>>,
}

impl MockGateway {
    fn responding_with(resp: http::Response<Body>) -> Self {
        Self {
            response: Mutex::new(Some(resp)),
        }
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for MockGateway {
    async fn create_upstream(
        &self,
        _: SecurityContext,
        _: oagw_sdk::CreateUpstreamRequest,
    ) -> Result<oagw_sdk::Upstream, CanonicalError> {
        unimplemented!()
    }
    async fn get_upstream(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
    ) -> Result<oagw_sdk::Upstream, CanonicalError> {
        unimplemented!()
    }
    async fn list_upstreams(
        &self,
        _: SecurityContext,
        _: &oagw_sdk::ListQuery,
    ) -> Result<Vec<oagw_sdk::Upstream>, CanonicalError> {
        unimplemented!()
    }
    async fn update_upstream(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
        _: oagw_sdk::UpdateUpstreamRequest,
    ) -> Result<oagw_sdk::Upstream, CanonicalError> {
        unimplemented!()
    }
    async fn delete_upstream(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
    ) -> Result<(), CanonicalError> {
        unimplemented!()
    }
    async fn create_route(
        &self,
        _: SecurityContext,
        _: oagw_sdk::CreateRouteRequest,
    ) -> Result<oagw_sdk::Route, CanonicalError> {
        unimplemented!()
    }
    async fn get_route(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
    ) -> Result<oagw_sdk::Route, CanonicalError> {
        unimplemented!()
    }
    async fn list_routes(
        &self,
        _: SecurityContext,
        _: Option<uuid::Uuid>,
        _: &oagw_sdk::ListQuery,
    ) -> Result<Vec<oagw_sdk::Route>, CanonicalError> {
        unimplemented!()
    }
    async fn update_route(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
        _: oagw_sdk::UpdateRouteRequest,
    ) -> Result<oagw_sdk::Route, CanonicalError> {
        unimplemented!()
    }
    async fn delete_route(&self, _: SecurityContext, _: uuid::Uuid) -> Result<(), CanonicalError> {
        unimplemented!()
    }
    async fn resolve_proxy_target(
        &self,
        _: SecurityContext,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(oagw_sdk::Upstream, oagw_sdk::Route), CanonicalError> {
        unimplemented!()
    }

    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        _req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        Ok(self
            .response
            .lock()
            .unwrap()
            .take()
            .expect("response already consumed"))
    }
}

// ---------------------------------------------------------------------------
// GatewayTransport — thin adapter over proxy_request
// ---------------------------------------------------------------------------

struct GatewayTransport<G> {
    gateway: Arc<G>,
    ctx: SecurityContext,
    alias: String,
}

impl<G: ServiceGatewayClientV1> GatewayTransport<G> {
    /// POST a JSON request body to `/{alias}/{path}` and deserialize the response.
    async fn post_json<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        request: &Req,
    ) -> Result<Resp, Box<dyn std::error::Error + Send + Sync>> {
        let body = serde_json::to_vec(request)?;
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("/{}/{}", self.alias, path))
            .header("content-type", "application/json")
            .body(Body::from(Bytes::from(body)))?;

        let resp = self.gateway.proxy_request(self.ctx.clone(), req).await?;
        let bytes = resp.into_body().into_bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// POST a JSON request and return the response as an SSE event stream.
    async fn post_stream<Req: Serialize>(
        &self,
        path: &str,
        request: &Req,
    ) -> Result<ServerEventsStream<ServerEvent>, Box<dyn std::error::Error + Send + Sync>> {
        let body = serde_json::to_vec(request)?;
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("/{}/{}", self.alias, path))
            .header("content-type", "application/json")
            .body(Body::from(Bytes::from(body)))?;

        let resp = self.gateway.proxy_request(self.ctx.clone(), req).await?;

        match ServerEventsStream::from_response::<ServerEvent>(resp) {
            ServerEventsResponse::Events(stream) => Ok(stream),
            ServerEventsResponse::Response(_) => {
                Err("expected SSE stream but got a non-streaming response".into())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn in_process_transport_chat_completion() -> TestResult {
    // -- setup: build request using async-openai types --------------------------
    let request = CreateChatCompletionRequestArgs::default()
        .model("gpt-4o")
        .messages(vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text("Say hello".into()),
                name: None,
            },
        )])
        .build()?;

    // Verify the request serializes correctly
    let json = serde_json::to_value(&request)?;
    assert_eq!(json["model"], "gpt-4o");
    assert_eq!(json["messages"][0]["role"], "user");

    // -- action: send through gateway transport ---------------------------------
    let gateway = MockGateway::responding_with(canned_response());
    let transport = GatewayTransport {
        gateway: Arc::new(gateway),
        ctx: SecurityContext::anonymous(),
        alias: "openai".to_string(),
    };

    let response: CreateChatCompletionResponse =
        transport.post_json("v1/chat/completions", &request).await?;

    // -- verify: response deserializes into async-openai types ------------------
    assert_eq!(response.id, "chatcmpl-test-123");
    assert_eq!(response.model, "gpt-4o");
    assert_eq!(response.choices.len(), 1);
    assert_eq!(
        response.choices[0].message.content.as_deref(),
        Some("Hello from OAGW!")
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// SSE streaming test — OpenAI streaming chat completion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn in_process_transport_streaming_chat_completion() -> TestResult {
    // -- setup: gateway returns an SSE stream of OpenAI chat completion chunks --
    let gateway = MockGateway::responding_with(server_events_response(vec![
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" from\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" OAGW!\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    ]));

    let request = CreateChatCompletionRequestArgs::default()
        .model("gpt-4o")
        .messages(vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text("Say hello".into()),
                name: None,
            },
        )])
        .build()?;

    let transport = GatewayTransport {
        gateway: Arc::new(gateway),
        ctx: SecurityContext::anonymous(),
        alias: "openai".to_string(),
    };

    // -- action: stream through gateway transport ---------------------------------
    let mut events = transport
        .post_stream("v1/chat/completions", &request)
        .await?;

    // -- verify: accumulate content deltas from streamed chunks --------------------
    let mut text = String::new();
    while let Some(result) = events.next().await {
        let ev = result?;
        if ev.data == "[DONE]" {
            break;
        }
        let chunk: serde_json::Value = serde_json::from_str(&ev.data)?;
        if let Some(content) = chunk["choices"][0]["delta"]["content"].as_str() {
            text.push_str(content);
        }
    }

    assert_eq!(text, "Hello from OAGW!");

    Ok(())
}
