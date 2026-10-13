//! Provider adapters (ADR-0005): one per `ProviderKind`.
//!
//! An adapter builds the wire body from an [`LlmRequest`], translates wire
//! SSE events into [`LlmEvent`]s and parses non-streaming responses. The
//! gateway picks the adapter with [`adapter_for`]; adding a provider kind
//! means adding an adapter module and a match arm there.

pub mod anthropic_messages;
pub mod openai_chat_completions;
pub mod openai_responses;
#[cfg(test)]
pub(crate) mod test_util;
pub mod vllm_responses;

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::config::ProviderKind;
use crate::infra::llm::sse_parser::SseEvent;
use crate::infra::llm::types::{LlmCompletion, LlmEvent, LlmRequest, ProviderFailure};

pub use anthropic_messages::AnthropicMessagesAdapter;
pub use openai_chat_completions::OpenAiChatCompletionsAdapter;
pub use openai_responses::OpenAiResponsesAdapter;
pub use vllm_responses::VllmResponsesAdapter;

/// Wire protocol of one provider kind.
pub trait ProviderAdapter: Send + Sync {
    /// Request body (JSON) for `req`.
    fn build_body(&self, req: &LlmRequest) -> Value;

    /// Protocol headers sent with the request besides the content type
    /// (none by default).
    fn extra_headers(&self, _req: &LlmRequest) -> Vec<(&'static str, String)> {
        Vec::new()
    }

    /// Translate one wire SSE event (possibly into several, or no, events).
    fn parse_event(&self, event: &SseEvent, state: &mut ParseState) -> Vec<LlmEvent>;

    /// Parse a non-streaming response body.
    ///
    /// # Errors
    /// A sanitized [`ProviderFailure`] when the body reports a failure or is
    /// not a valid response.
    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderFailure>;
}

/// Per-stream parser state kept by the gateway across `parse_event` calls.
#[derive(Debug, Default)]
pub struct ParseState {
    /// Answer text so far per `(output_index, content_index)` (citation snippets).
    pub(crate) text_parts: HashMap<(u64, u64), String>,
    /// Output items whose annotations were already streamed.
    pub(crate) annotated_items: HashSet<u64>,
    /// Provider response id seen so far (Chat Completions chunk id,
    /// Anthropic message id).
    pub(crate) response_id: Option<String>,
    /// Wire usage object seen so far (Anthropic: `message_start` usage
    /// overlaid with `message_delta` usage).
    pub(crate) usage: Option<Value>,
    /// Finish / stop reason seen so far.
    pub(crate) stop_reason: Option<String>,
    /// Function calls being streamed, by tool-call index (Chat Completions)
    /// or content block index (Anthropic).
    pub(crate) calls: BTreeMap<u64, PendingCall>,
    /// Anthropic content blocks of code execution calls (their stop is the
    /// tool `done`).
    pub(crate) code_blocks: HashSet<u64>,
    /// vLLM: inside a `<think>` block.
    pub(crate) in_think: bool,
    /// vLLM: text held back because it may be the start of a tag.
    pub(crate) think_pending: String,
}

/// A function call whose arguments are still streaming.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PendingCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

/// Adapter serving `kind`.
#[must_use]
pub fn adapter_for(kind: ProviderKind) -> &'static dyn ProviderAdapter {
    match kind {
        ProviderKind::OpenaiResponses => &OpenAiResponsesAdapter,
        ProviderKind::OpenaiChatCompletions => &OpenAiChatCompletionsAdapter,
        ProviderKind::VllmResponses => &VllmResponsesAdapter,
        ProviderKind::AnthropicMessages => &AnthropicMessagesAdapter,
    }
}
