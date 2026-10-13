//! `llm_provider` library (DESIGN section 3.2, ADR-0001, ADR-0005).
//!
//! - [`resolver`] — maps a catalog model's `provider_id` and the tenant to a
//!   [`ProviderTarget`];
//! - [`openai_responses`] — the `openai_responses` adapter;
//! - [`openai_chat_completions`] — the `openai_chat_completions` adapter;
//! - [`vllm_responses`] — the `vllm_responses` adapter;
//! - [`anthropic_messages`] — the `anthropic_messages` adapter;
//! - [`transport`] — the OAGW request path and error mapping shared by the
//!   adapters;
//! - [`sse`] — incremental SSE parsing and the provider event stream driver;
//! - [`storage`] — the provider Files / Vector Stores clients (RAG storage),
//!   dispatched by `storage_kind` / `rag_provider`;
//! - [`anthropic_files`] — the Anthropic Files API client (secondary image
//!   copies of Anthropic chats);
//! - [`knowledge`] — the Azure `OpenAI` knowledge retriever (knowledge
//!   search).
//!
//! [`LlmAdapters`] implements [`LlmProvider`] by dispatching on the target's
//! [`ProviderKind`]. Adding an adapter means adding its field, its arm in
//! [`LlmAdapters::adapter`] and in [`LlmAdapters::supports`]; the resolver
//! rejects kinds without an adapter before any stream opens.

pub mod anthropic_files;
pub mod anthropic_messages;
pub mod knowledge;
pub mod openai_chat_completions;
pub mod openai_responses;
pub mod resolver;
pub mod sse;
pub mod storage;
pub mod transport;
pub mod vllm_responses;

#[cfg(test)]
#[path = "fake_gateway_tests.rs"]
pub(crate) mod fake_gateway;

use std::sync::Arc;

use async_trait::async_trait;
use oagw_sdk::api::ServiceGatewayClientV1;
use tokio_util::sync::CancellationToken;

use crate::config::ProviderKind;
use crate::domain::llm::{
    LlmCompletion, LlmEventStream, LlmFailure, LlmProvider, LlmRequest, ProviderTarget,
};
use crate::infra::oagw_provisioning::SystemContextSource;

use self::anthropic_messages::AnthropicMessagesAdapter;
use self::openai_chat_completions::OpenAiChatCompletionsAdapter;
use self::openai_responses::OpenAiResponsesAdapter;
use self::vllm_responses::VllmResponsesAdapter;

/// Configuration name of a provider kind.
#[must_use]
pub const fn kind_name(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenaiResponses => "openai_responses",
        ProviderKind::OpenaiChatCompletions => "openai_chat_completions",
        ProviderKind::VllmResponses => "vllm_responses",
        ProviderKind::AnthropicMessages => "anthropic_messages",
    }
}

/// Every adapter, selected by provider kind.
#[derive(Debug)]
pub struct LlmAdapters {
    openai_responses: OpenAiResponsesAdapter,
    openai_chat_completions: OpenAiChatCompletionsAdapter,
    vllm_responses: VllmResponsesAdapter,
    anthropic_messages: AnthropicMessagesAdapter,
}

impl LlmAdapters {
    /// Adapters over the OAGW client and the S2S context source.
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        system: Arc<dyn SystemContextSource>,
    ) -> Self {
        Self {
            openai_responses: OpenAiResponsesAdapter::new(
                Arc::clone(&gateway),
                Arc::clone(&system),
            ),
            openai_chat_completions: OpenAiChatCompletionsAdapter::new(
                Arc::clone(&gateway),
                Arc::clone(&system),
            ),
            vllm_responses: VllmResponsesAdapter::new(Arc::clone(&gateway), Arc::clone(&system)),
            anthropic_messages: AnthropicMessagesAdapter::new(gateway, system),
        }
    }

    /// Whether `kind` has an adapter (every configured kind has one).
    #[must_use]
    pub const fn supports(kind: ProviderKind) -> bool {
        match kind {
            ProviderKind::OpenaiResponses
            | ProviderKind::OpenaiChatCompletions
            | ProviderKind::VllmResponses
            | ProviderKind::AnthropicMessages => true,
        }
    }

    fn adapter(&self, kind: ProviderKind) -> &dyn LlmProvider {
        match kind {
            ProviderKind::OpenaiResponses => &self.openai_responses,
            ProviderKind::OpenaiChatCompletions => &self.openai_chat_completions,
            ProviderKind::VllmResponses => &self.vllm_responses,
            ProviderKind::AnthropicMessages => &self.anthropic_messages,
        }
    }
}

#[async_trait]
impl LlmProvider for LlmAdapters {
    async fn stream(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<LlmEventStream, LlmFailure> {
        self.adapter(target.kind).stream(target, req, cancel).await
    }

    async fn complete(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
    ) -> Result<LlmCompletion, LlmFailure> {
        self.adapter(target.kind).complete(target, req).await
    }
}
