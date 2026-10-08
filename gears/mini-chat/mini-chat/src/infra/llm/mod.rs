//! LLM provider library: provider resolver, S2S context, OAGW provisioning and the chat
//! adapters. File storage and vector stores live in `infra::storage`.

use std::sync::Arc;

use oagw_sdk::ServiceGatewayClientV1;

pub mod anthropic;
pub mod errors;
pub mod openai_chat;
pub mod openai_responses;
pub mod provisioning;
pub mod resolver;
pub mod s2s;
mod transport;
pub mod types;
pub mod vllm_responses;

pub use crate::config::{ProviderKind, StorageKind};
pub use resolver::{ChatTarget, ProviderResolver, StorageTarget};
pub use s2s::S2sContext;
pub use types::{
    ChatAdapter, CompletionResult, ContentPart, InputItem, ProviderError, ProviderErrorKind,
    ProviderEvent, ProviderEventStream, ProviderRequest, ProviderUsage, RawCitation,
    RequestMetadata, Role, ToolSpec,
};

use anthropic::AnthropicAdapter;
use openai_chat::OpenAiChatAdapter;
use openai_responses::OpenAiResponsesAdapter;
use vllm_responses::VllmResponsesAdapter;

/// Entry point to the chat adapters; every OAGW call is made with the S2S context.
pub struct LlmClient {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: S2sContext,
}

impl LlmClient {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: S2sContext) -> Self {
        Self { gateway, s2s }
    }

    /// The adapter that speaks the protocol of `kind`.
    #[must_use]
    pub fn adapter(&self, kind: ProviderKind) -> Arc<dyn ChatAdapter> {
        let (gateway, s2s) = (Arc::clone(&self.gateway), self.s2s.clone());
        match kind {
            ProviderKind::OpenaiResponses => Arc::new(OpenAiResponsesAdapter::new(gateway, s2s)),
            ProviderKind::OpenaiChatCompletions => Arc::new(OpenAiChatAdapter::new(gateway, s2s)),
            ProviderKind::VllmResponses => Arc::new(VllmResponsesAdapter::new(gateway, s2s)),
            ProviderKind::AnthropicMessages => Arc::new(AnthropicAdapter::new(gateway, s2s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use http::Method;

    use super::*;
    use crate::test_support::authn::s2s_security_context;
    use crate::test_support::fixtures::{chat_target, provider_request};
    use crate::test_support::gateway::{FakeGateway, Responder};

    /// The recorded request of one call of the adapter `client` selects for `kind`.
    async fn first_request(kind: ProviderKind) -> crate::test_support::gateway::RecordedRequest {
        let gateway = Arc::new(FakeGateway::new());
        gateway.on(Method::POST, "/v1/responses", Responder::Sse(vec![]));
        let s2s = S2sContext::new();
        s2s.set(s2s_security_context());
        let client = LlmClient::new(Arc::clone(&gateway) as _, s2s);
        if let Ok(events) = client
            .adapter(kind)
            .stream(&chat_target(kind), provider_request())
            .await
        {
            let _ = events.count().await;
        }
        gateway.requests().into_iter().next().expect("one request")
    }

    #[tokio::test]
    async fn adapter_is_selected_by_provider_kind() {
        let body = |r: &crate::test_support::gateway::RecordedRequest| r.json.clone().unwrap();

        let responses = first_request(ProviderKind::OpenaiResponses).await;
        assert!(body(&responses).get("metadata").is_some());

        let chat = first_request(ProviderKind::OpenaiChatCompletions).await;
        assert!(body(&chat).get("messages").is_some());
        assert!(body(&chat).get("stream_options").is_some());

        let vllm = first_request(ProviderKind::VllmResponses).await;
        assert!(body(&vllm).get("input").is_some());
        assert!(body(&vllm).get("metadata").is_none());

        let anthropic = first_request(ProviderKind::AnthropicMessages).await;
        assert!(anthropic.headers.get("anthropic-version").is_some());
        assert!(body(&anthropic).get("max_tokens").is_some());
    }
}
