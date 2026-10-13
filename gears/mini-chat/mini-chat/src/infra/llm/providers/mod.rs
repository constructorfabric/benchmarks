//! Provider adapters behind OAGW
//! (`cpt-cf-mini-chat-adr-multi-provider-adapters`).
//!
//! An adapter builds the request body of its wire format and creates one
//! [`EventTranslator`] per response stream. Slice 7 adds `openai_chat_completions`,
//! `vllm_responses` and `anthropic_messages` by implementing [`ProviderAdapter`]
//! and registering it in [`ProviderAdapters::standard`]; nothing else changes:
//! the client, the parser, the classification and the task are shared.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::ProviderKind;
use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::{BuiltRequest, ToolCallOutput};
use crate::domain::service::stream::request::ProviderRequestInput;
use crate::domain::service::stream::types::ProviderEvent;
use crate::infra::llm::sse_parse::SseEvent;

pub mod anthropic_messages;
pub mod chat_completions;
pub mod openai_responses;
pub mod vllm_responses;

#[cfg(test)]
pub(crate) mod test_fixtures;

/// The description of the `search_knowledge` function tool.
pub(crate) const SEARCH_KNOWLEDGE_DESCRIPTION: &str =
    "Searches the knowledge base and returns the most relevant passages.";

/// The JSON schema of the `search_knowledge` parameters.
pub(crate) fn search_knowledge_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "What to search for." },
            "top_k": { "type": "integer", "description": "How many passages to return." }
        },
        "required": ["query"],
    })
}

/// The event name of a Responses or Messages event: the SSE `event:` line, or
/// the `type` field of the `data` JSON when the line is missing, empty or
/// `message`.
pub(crate) fn event_name(event: &SseEvent, json: Option<&Value>) -> Option<String> {
    match (event.event.as_deref(), json) {
        (Some(name), _) if !name.is_empty() && name != "message" => Some(name.to_owned()),
        (_, Some(json)) => json["type"].as_str().map(str::to_owned),
        _ => None,
    }
}

/// `/{alias}{api_path}` with `{model}` replaced and the query kept.
pub(crate) fn request_uri(input: &ProviderRequestInput<'_>) -> String {
    format!(
        "/{}{}",
        input.target.alias,
        input
            .target
            .api_path
            .replace("{model}", &input.entry.provider_model_id)
    )
}

/// The `user` value: the two simple UUIDs, tenant first.
pub(crate) fn user_value(input: &ProviderRequestInput<'_>) -> String {
    format!("{}{}", input.tenant_id.simple(), input.user_id.simple())
}

/// `Content-Type` and `Accept` (never a credential).
pub(crate) fn base_headers() -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "text/event-stream".to_owned()),
    ]
}

/// Translates the wire events of one response stream into internal events.
pub trait EventTranslator: Send {
    /// Translates one parsed SSE event into zero or more internal events.
    fn translate(&mut self, event: &SseEvent) -> Vec<ProviderEvent>;

    /// Called once when the body ended: releases a pending outcome (Chat
    /// Completions after `[DONE]`); the Responses adapters return nothing.
    fn finish(&mut self) -> Vec<ProviderEvent>;
}

/// One provider kind.
pub trait ProviderAdapter: Send + Sync {
    /// The kind the adapter serves.
    fn kind(&self) -> ProviderKind;

    /// Builds the request of a turn.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the body cannot be serialized.
    fn build_request(&self, input: &ProviderRequestInput<'_>) -> Result<BuiltRequest, DomainError>;

    /// The next request of the knowledge-search loop: `previous` with the
    /// function calls and their outputs appended to the input in the format of
    /// this adapter (`inst-append-output`).
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the adapter has no continuation or the
    /// body of `previous` has no input array of its format.
    fn continue_request(
        &self,
        _previous: &BuiltRequest,
        _outputs: &[ToolCallOutput],
    ) -> Result<BuiltRequest, DomainError> {
        Err(DomainError::Internal(
            "the adapter has no request continuation".to_owned(),
        ))
    }

    /// A fresh translator for one response stream.
    fn translator(&self) -> Box<dyn EventTranslator>;
}

/// The adapters by kind.
#[derive(Default)]
pub struct ProviderAdapters {
    adapters: Vec<Arc<dyn ProviderAdapter>>,
}

impl ProviderAdapters {
    /// No adapter registered.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    // @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-other-adapters:p1
    /// The adapters of this build: the four provider kinds.
    #[must_use]
    pub fn standard() -> Self {
        let mut adapters = Self::empty();
        adapters.register(Arc::new(openai_responses::OpenAiResponsesAdapter::new()));
        adapters.register(Arc::new(chat_completions::ChatCompletionsAdapter::new()));
        adapters.register(Arc::new(vllm_responses::VllmResponsesAdapter::new()));
        adapters.register(Arc::new(anthropic_messages::AnthropicMessagesAdapter::new()));
        adapters
    }

    /// Registers an adapter; a later one of the same kind wins.
    pub fn register(&mut self, adapter: Arc<dyn ProviderAdapter>) {
        self.adapters.retain(|known| known.kind() != adapter.kind());
        self.adapters.push(adapter);
    }

    /// The adapter of `kind`.
    #[must_use]
    pub fn get(&self, kind: ProviderKind) -> Option<&Arc<dyn ProviderAdapter>> {
        self.adapters.iter().find(|adapter| adapter.kind() == kind)
    }
}
