//! LLM provider layer (DESIGN §3.2 `llm_provider`, ADR-0005): provider
//! adapters, the provider resolver and the [`client::LlmClient`] that sends
//! requests through the OAGW in-process proxy.

#[cfg(test)]
mod adapter_fixtures;
pub mod anthropic_files;
pub mod anthropic_messages;
pub mod client;
pub mod knowledge;
pub mod openai_chat_completions;
pub mod openai_responses;
pub mod resolver;
pub mod sse_reader;
pub mod storage;
pub mod types;
pub mod vllm_responses;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::config::ProviderKind;

pub use anthropic_files::AnthropicFilesClient;
pub use anthropic_messages::AnthropicMessagesAdapter;
pub use client::LlmClient;
pub use knowledge::{AzureKnowledgeRetriever, KnowledgeRetriever, KnowledgeSearch, KnowledgeTurn};
pub use openai_chat_completions::OpenAiChatCompletionsAdapter;
pub use openai_responses::OpenAiResponsesAdapter;
pub use resolver::{ProviderResolver, ResolvedProvider};
pub use storage::{RagClient, StorageError, VsFileStatus};
pub use types::{
    FunctionCall, LlmError, LlmEvent, LlmMessage, LlmRequest, LlmTool, LlmUsage, RawCitation,
    RequestMetadata, RequestType, ToolResult, ToolRound,
};
pub use vllm_responses::VllmResponsesAdapter;

/// Result of a non-streaming call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub text: String,
    pub usage: Option<LlmUsage>,
}

/// Per-stream parser state of an adapter.
#[derive(Debug, Default)]
pub struct ParseState {
    /// Provider response id (`response.id`).
    pub response_id: Option<String>,
    /// Accumulated text per output text part (citation snippets).
    pub part_texts: HashMap<String, String>,
    /// Dedupe keys of citations already emitted.
    pub citations_seen: HashSet<String>,
    /// Usage reported before the terminal event (Chat Completions, Anthropic).
    pub usage: Option<LlmUsage>,
    /// Finish / stop reason reported before the terminal event.
    pub finish_reason: Option<String>,
    /// Function calls being streamed, by call index (Chat Completions) or
    /// content block index (Anthropic).
    pub calls: BTreeMap<u64, FunctionCall>,
    /// Anthropic content blocks: index -> tool name of a started tool block.
    pub blocks: HashMap<u64, String>,
    /// The terminal event was emitted.
    pub terminated: bool,
    /// vLLM: inside a `<think>` block.
    pub in_think: bool,
    /// vLLM: text held back because it may start a `<think>` / `</think>` tag.
    pub held: String,
}

/// Wire protocol of one provider kind.
pub trait ProviderAdapter: Send + Sync {
    /// Request body for `req`.
    fn build_body(&self, req: &LlmRequest) -> Value;
    /// Extra HTTP headers of the request for `req`.
    fn headers(&self, _req: &LlmRequest) -> Vec<(&'static str, String)> {
        Vec::new()
    }
    /// Translate one SSE event (`event` is the `event:` line, empty when absent).
    fn parse_event(&self, st: &mut ParseState, event: &str, data: &str) -> Vec<LlmEvent>;
    /// Parse a non-streaming response body.
    ///
    /// # Errors
    /// The provider reported a failure or the body is not a valid response.
    fn parse_completion(&self, body: &[u8]) -> Result<CompletionResult, LlmError>;
}

/// Adapter serving `kind` (ADR-0005).
#[must_use]
pub fn adapter_for(kind: ProviderKind) -> Arc<dyn ProviderAdapter> {
    match kind {
        ProviderKind::OpenaiResponses => Arc::new(OpenAiResponsesAdapter),
        ProviderKind::OpenaiChatCompletions => Arc::new(OpenAiChatCompletionsAdapter),
        ProviderKind::VllmResponses => Arc::new(VllmResponsesAdapter),
        ProviderKind::AnthropicMessages => Arc::new(AnthropicMessagesAdapter),
    }
}

/// Description of the `search_knowledge` function tool.
pub(crate) const SEARCH_KNOWLEDGE_DESCRIPTION: &str =
    "Search the knowledge base for passages relevant to the query.";

/// JSON schema of the `search_knowledge` arguments.
pub(crate) fn search_knowledge_parameters() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Search query."},
            "top_k": {"type": "integer", "description": "Maximum number of passages to return."},
        },
        "required": ["query"],
        "additionalProperties": false,
    })
}

/// Merge `extra_body` into `body`, skipping keys the request controls (with a
/// warning); used by every adapter but Anthropic.
pub(crate) fn merge_extra_body(
    body: &mut serde_json::Map<String, Value>,
    extra: Option<&serde_json::Map<String, Value>>,
) {
    for (k, v) in extra.into_iter().flatten() {
        if openai_responses::CONTROLLED_KEYS.contains(&k.as_str()) {
            tracing::warn!(key = %k, "extra_body key controlled by the request is ignored");
        } else {
            body.insert(k.clone(), v.clone());
        }
    }
}
