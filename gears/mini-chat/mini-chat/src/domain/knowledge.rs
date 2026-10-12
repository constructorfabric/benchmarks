//! Knowledge search (DESIGN section 4 "Knowledge Search", Appendix B.7
//! "Knowledge search", "Streaming error codes").
//!
//! The model queries the organization knowledge base (one Azure `OpenAI`
//! vector store of the deployment) through the `search_knowledge` function
//! tool. This module holds the provider-neutral parts:
//!
//! - the [`KnowledgeRetriever`] port (Azure `OpenAI` implementation in
//!   [`crate::infra::llm::knowledge`]), wired only when
//!   `knowledge_search.enabled = true`;
//! - [`KnowledgeTurn::for_request`]: the per-request parameters. They are not
//!   built (knowledge search is off for the request, with a warning) when the
//!   retriever is missing or reports that the `knowledge_search.provider_id`
//!   entry cannot serve the tenant; the stream service also leaves them out
//!   when `file_search` is sent (the two tools are never sent together);
//! - the [`search_knowledge_tool`] definition (`query`, optional `top_k`);
//! - the tool-call handling of the agentic loop: argument parsing with
//!   `top_k` capped at `knowledge_search.top_k`, chunk trimming to
//!   `knowledge_search.max_chunk_chars` and the `function_call_output` texts
//!   (results, "search limit reached", failure, invalid arguments).
//!
//! The loop itself (one provider request per iteration, the
//! `max_calls_per_message` retrieval limit and the `max_calls_per_message +
//! 2` iteration cap) runs in the provider task, see
//! [`crate::domain::stream::task`].

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::KnowledgeSearchConfig;
use crate::domain::llm::LlmFunctionTool;

/// Name of the knowledge-search function tool.
pub const SEARCH_KNOWLEDGE_TOOL: &str = "search_knowledge";

/// Description of the `search_knowledge` tool sent to the model.
pub const SEARCH_KNOWLEDGE_DESCRIPTION: &str =
    "Search the organization knowledge base and return the most relevant passages for a query.";

/// Output of a call above `knowledge_search.max_calls_per_message`.
pub const LIMIT_REACHED_MESSAGE: &str =
    "Search limit reached for this message. Answer from the passages you already have.";

/// Output of a failed retrieval.
pub const SEARCH_FAILED_MESSAGE: &str = "The knowledge base search failed. Answer without it and say that the knowledge base was not available.";

/// Output of a call whose arguments carry no usable `query`.
pub const INVALID_ARGUMENTS_MESSAGE: &str = "Invalid arguments: search_knowledge needs a non-empty string `query` and an optional positive integer `top_k`.";

/// Output note of a retrieval without results.
pub const NO_RESULTS_MESSAGE: &str = "No relevant passages were found in the knowledge base.";

/// The `search_knowledge` function tool; `top_k` is described with its cap.
#[must_use]
pub fn search_knowledge_tool(top_k_cap: usize) -> LlmFunctionTool {
    LlmFunctionTool {
        name: SEARCH_KNOWLEDGE_TOOL.to_owned(),
        description: SEARCH_KNOWLEDGE_DESCRIPTION.to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to look up in the knowledge base.",
                },
                "top_k": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": top_k_cap,
                    "description": format!("How many passages to return (at most {top_k_cap})."),
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
    }
}

// ── Port ─────────────────────────────────────────────────────────────────

/// One passage returned by the knowledge base.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeChunk {
    pub text: String,
    /// Source file name, when the store reported one.
    pub filename: Option<String>,
    pub score: Option<f64>,
}

/// A failed retrieval; the text is a diagnostic for logs only (never sent to
/// clients or to the model).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("knowledge search failed: {0}")]
pub struct KnowledgeSearchError(pub String);

/// Knowledge retriever port: the vector store search of the configured
/// knowledge base, sent through OAGW.
#[async_trait]
pub trait KnowledgeRetriever: Send + Sync {
    /// Whether the knowledge-search parameters can be built for `tenant_id`:
    /// the `knowledge_search.provider_id` entry exists, is of kind
    /// `openai_responses` or `anthropic_messages`, has an upstream alias for
    /// the tenant and a non-empty `api_version`.
    ///
    /// # Errors
    ///
    /// The reason (for the warning) when they cannot.
    fn check(&self, tenant_id: Uuid) -> Result<(), String>;

    /// Searches the knowledge base for `query`, asking for at most
    /// `max_num_results` passages.
    ///
    /// # Errors
    ///
    /// [`KnowledgeSearchError`] on any gateway, provider or response failure.
    async fn search(
        &self,
        tenant_id: Uuid,
        query: &str,
        max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, KnowledgeSearchError>;
}

// ── Per-request parameters ───────────────────────────────────────────────

/// Limits of the knowledge-search loop (`knowledge_search.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnowledgeLimits {
    /// `max_calls_per_message`: retrievals per message.
    pub max_calls: u32,
    /// `top_k`: cap of the model's `top_k` (and its default).
    pub top_k: usize,
    /// `max_chunk_chars`: characters kept of each passage.
    pub max_chunk_chars: usize,
}

impl KnowledgeLimits {
    /// The limits of the configuration.
    #[must_use]
    pub const fn from_config(config: &KnowledgeSearchConfig) -> Self {
        Self {
            max_calls: config.max_calls_per_message,
            top_k: config.top_k,
            max_chunk_chars: config.max_chunk_chars,
        }
    }

    /// Hard cap of provider requests per turn: `max_calls_per_message + 2`.
    #[must_use]
    pub const fn max_iterations(&self) -> u32 {
        self.max_calls.saturating_add(2)
    }
}

/// Knowledge search of one turn: `search_knowledge` is offered and its calls
/// are served by the retriever.
#[derive(Clone)]
pub struct KnowledgeTurn {
    pub retriever: Arc<dyn KnowledgeRetriever>,
    /// Tenant of the turn (selects the provider alias).
    pub tenant_id: Uuid,
    pub limits: KnowledgeLimits,
}

impl std::fmt::Debug for KnowledgeTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnowledgeTurn")
            .field("tenant_id", &self.tenant_id)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl KnowledgeTurn {
    /// The knowledge-search parameters of a request of `tenant_id`, or
    /// `None` (knowledge search off for the request): disabled, or, with a
    /// warning, no retriever or a provider entry that cannot serve the
    /// tenant (module docs).
    #[must_use]
    pub fn for_request(
        config: &KnowledgeSearchConfig,
        retriever: Option<&Arc<dyn KnowledgeRetriever>>,
        tenant_id: Uuid,
    ) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let Some(retriever) = retriever else {
            tracing::warn!(
                target: "mini_chat.knowledge",
                "knowledge search is enabled but no knowledge retriever is configured; search_knowledge is not offered"
            );
            return None;
        };
        if let Err(reason) = retriever.check(tenant_id) {
            tracing::warn!(
                target: "mini_chat.knowledge",
                tenant_id = %tenant_id,
                reason = %reason,
                "knowledge search parameters cannot be built; search_knowledge is not offered"
            );
            return None;
        }
        Some(Self {
            retriever: Arc::clone(retriever),
            tenant_id,
            limits: KnowledgeLimits::from_config(config),
        })
    }
}

// ── Tool-call handling ───────────────────────────────────────────────────

/// Parsed arguments of a `search_knowledge` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchArguments {
    pub query: String,
    /// The model's `top_k` capped at `knowledge_search.top_k` (that cap
    /// when absent or not a positive number).
    pub top_k: usize,
}

/// Parses the JSON arguments of a call; `None` without a non-blank string
/// `query`.
#[must_use]
pub fn parse_arguments(arguments: &str, top_k_cap: usize) -> Option<SearchArguments> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    let query = value.get("query")?.as_str()?.trim();
    if query.is_empty() {
        return None;
    }
    let cap = top_k_cap.max(1);
    let requested = value.get("top_k").and_then(|top_k| {
        top_k
            .as_u64()
            .or_else(|| top_k.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
    });
    let top_k = requested
        .filter(|top_k| *top_k > 0)
        .map_or(cap, |top_k| usize::try_from(top_k).unwrap_or(cap).min(cap));
    Some(SearchArguments {
        query: query.to_owned(),
        top_k,
    })
}

/// The first `max_chars` characters of `text`.
#[must_use]
pub fn trim_chunk(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// The `function_call_output` of a successful retrieval: at most `top_k`
/// passages, each trimmed to `max_chunk_chars` characters.
#[must_use]
pub fn results_output(chunks: &[KnowledgeChunk], top_k: usize, max_chunk_chars: usize) -> String {
    let results: Vec<Value> = chunks
        .iter()
        .take(top_k)
        .enumerate()
        .map(|(index, chunk)| {
            let mut result = json!({
                "index": index + 1,
                "text": trim_chunk(&chunk.text, max_chunk_chars),
            });
            if let Some(filename) = &chunk.filename {
                result["source"] = json!(filename);
            }
            result
        })
        .collect();
    let output = if results.is_empty() {
        json!({ "results": results, "message": NO_RESULTS_MESSAGE })
    } else {
        json!({ "results": results })
    };
    output.to_string()
}

/// The output of a call above `max_calls_per_message`.
#[must_use]
pub fn limit_reached_output() -> String {
    message_output("search limit reached", LIMIT_REACHED_MESSAGE)
}

/// The output of a failed retrieval.
#[must_use]
pub fn search_failed_output() -> String {
    message_output("search failed", SEARCH_FAILED_MESSAGE)
}

/// The output of a call with unusable arguments.
#[must_use]
pub fn invalid_arguments_output() -> String {
    message_output("invalid arguments", INVALID_ARGUMENTS_MESSAGE)
}

fn message_output(error: &str, message: &str) -> String {
    json!({ "error": error, "message": message }).to_string()
}

#[cfg(test)]
#[path = "knowledge_tests.rs"]
mod tests;
