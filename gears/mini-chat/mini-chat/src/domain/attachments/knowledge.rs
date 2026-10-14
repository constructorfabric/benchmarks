//! The knowledge search agentic loop
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop`): answers each
//! `search_knowledge` tool call of a turn through the knowledge retriever (OAGW,
//! `POST /{alias}/openai/vector_stores/{id}/search`), caps `top_k`, trims the chunks, runs at most
//! `knowledge_search.max_calls_per_message` retrievals per message (later calls get a limit
//! output), caps the provider iterations at that value plus 2 (`agentic_iterations_exceeded`),
//! ends a call of any other tool name with `unexpected_tool_use`, counts the successful
//! retrievals in `chat_turns.file_search_completed_count` (through `TurnProgress`) and records
//! the knowledge series. The provider task owns the provider calls; it asks the loop how to
//! answer each tool-use outcome and settles only the final iteration.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::KnowledgeSearchConfig;
use crate::domain::attachments::contributions::{SEARCH_KNOWLEDGE, ToolResultFormat};
use crate::domain::attachments::metrics::AttachmentMetrics;
use crate::domain::error::StreamErrorCode;
use crate::domain::streaming::progress::TurnProgress;
use crate::domain::streaming::tools::ToolKind;

/// The output of a call that exceeds `knowledge_search.max_calls_per_message`; it contains the
/// text "search limit reached".
pub const SEARCH_LIMIT_OUTPUT: &str = "search limit reached";

/// The longest one retrieval may take (the OAGW call and the read of its answer): a slower
/// search is a failed retrieval. Well below the orphan watchdog timeout (90 s at least).
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The loop values (`knowledge_search.*`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnowledgeLoopSettings {
    pub max_calls_per_message: u32,
    pub top_k: usize,
    pub max_chunk_chars: usize,
    /// The bound of one retrieval ([`SEARCH_TIMEOUT`] by default).
    pub search_timeout: Duration,
}

impl KnowledgeLoopSettings {
    /// The values of the validated configuration.
    #[must_use]
    pub fn from_config(config: &KnowledgeSearchConfig) -> Self {
        Self {
            max_calls_per_message: config.max_calls_per_message,
            top_k: config.top_k,
            max_chunk_chars: config.max_chunk_chars,
            search_timeout: SEARCH_TIMEOUT,
        }
    }

    /// The hard cap of provider iterations of one turn: `max_calls_per_message` plus 2.
    #[must_use]
    pub const fn max_iterations(&self) -> u32 {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-iterations
        self.max_calls_per_message.saturating_add(2)
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-iterations
    }
}

/// A retrieved chunk (the text of one search result).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeChunk {
    pub text: String,
}

/// Why a retrieval failed.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalFailure {
    /// A gateway failure or a transport error.
    Transport,
    /// A non-2xx answer.
    Status(u16),
    /// A 2xx answer that is not a search result page.
    Malformed,
    /// The search did not answer within its time bound.
    Timeout,
}

/// The knowledge retriever port; the adapter is `infra::oagw::knowledge_client`.
#[async_trait]
pub trait KnowledgeRetriever: Send + Sync {
    /// Search the organization knowledge base for `query`, at most `top_k` chunks, under the
    /// gear's S2S context for the caller's tenant.
    ///
    /// # Errors
    ///
    /// [`RetrievalFailure`] when the search does not produce a result page.
    async fn search(
        &self,
        tenant_id: Uuid,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<KnowledgeChunk>, RetrievalFailure>;
}

/// A tool call of a tool-use outcome.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    pub call_id: String,
    /// The raw JSON text of the arguments (`{"query": "...", "top_k": 3}`).
    pub arguments: String,
}

/// What the provider task does with a tool-use outcome.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopStep {
    /// Append the call and this output to the provider input and issue the next provider
    /// request with the same tools.
    Continue { output: String },
    /// End the turn.
    Stop(LoopStop),
}

/// Why the loop ends the turn.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStop {
    /// Hand the outcome `failed` with this code to the finalization (`unexpected_tool_use` or
    /// `agentic_iterations_exceeded`).
    Failed(StreamErrorCode),
    /// The turn's cancellation token fired; the disconnect path finalizes the turn.
    Cancelled,
}

/// The collaborators of the loop.
#[domain_model]
#[derive(Clone)]
pub struct KnowledgeLoopDeps {
    pub retriever: Arc<dyn KnowledgeRetriever>,
    /// The guarded `file_search_completed_count` increment.
    pub progress: Arc<dyn TurnProgress>,
    pub metrics: Arc<dyn AttachmentMetrics>,
    pub settings: KnowledgeLoopSettings,
}

/// The loop state of one turn: the provider iterations issued so far (the first one counts) and
/// the retrievals made.
#[domain_model]
pub struct KnowledgeLoop {
    deps: KnowledgeLoopDeps,
    tenant_id: Uuid,
    turn_id: Uuid,
    iterations: u32,
    retrievals: u32,
}

impl KnowledgeLoop {
    /// The loop of a turn whose first provider iteration was issued.
    #[must_use]
    pub fn new(deps: KnowledgeLoopDeps, tenant_id: Uuid, turn_id: Uuid) -> Self {
        Self {
            deps,
            tenant_id,
            turn_id,
            iterations: 1,
            retrievals: 0,
        }
    }

    /// The provider iterations issued so far.
    #[must_use]
    pub fn iterations(&self) -> u32 {
        self.iterations
    }

    /// The in-memory file search count (reported as `file_search_calls` in the usage and audit
    /// events): the retrievals attempted, successful or not.
    #[must_use]
    pub fn file_search_calls(&self) -> u32 {
        self.retrievals
    }

    /// Decide the next step after the iteration ended with the tool-use outcome `call`.
    pub async fn answer(&mut self, call: &ToolCall, cancel: &CancellationToken) -> LoopStep {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-unexpected
        if call.name != SEARCH_KNOWLEDGE {
            return LoopStep::Stop(LoopStop::Failed(StreamErrorCode::UnexpectedToolUse));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-unexpected
        let settings = self.deps.settings;
        let output = if self.retrievals >= settings.max_calls_per_message {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-limit-output
            SEARCH_LIMIT_OUTPUT.to_owned()
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-limit-output
        } else {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-retrieve
            self.retrievals += 1;
            // a cancellation of the turn ends the wait for the search
            let Some(output) = self.retrieve(call, cancel).await else {
                return LoopStep::Stop(LoopStop::Cancelled);
            };
            output
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-retrieve
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-cap
        if self.iterations.saturating_add(1) > settings.max_iterations() {
            return LoopStep::Stop(LoopStop::Failed(StreamErrorCode::AgenticIterationsExceeded));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-cap
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-cancelled
        if cancel.is_cancelled() {
            return LoopStep::Stop(LoopStop::Cancelled);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-cancelled
        self.iterations += 1;
        LoopStep::Continue { output }
    }

    /// One search for the call's `query` (`top_k` capped), bounded by `search_timeout`: a slower
    /// one is a failed retrieval. `None` when `cancel` fired while the search ran.
    async fn bounded_search(
        &self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Option<Result<Vec<KnowledgeChunk>, RetrievalFailure>> {
        let deps = &self.deps;
        let arguments: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let top_k = arguments
            .get("top_k")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .map_or(deps.settings.top_k, |asked| {
                asked.clamp(1, deps.settings.top_k)
            });
        let search = tokio::time::timeout(
            deps.settings.search_timeout,
            deps.retriever.search(self.tenant_id, &query, top_k),
        );
        tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = search => Some(result.unwrap_or(Err(RetrievalFailure::Timeout))),
        }
    }

    /// The answer of one retrieval, `None` when `cancel` fired while the search ran.
    async fn retrieve(&self, call: &ToolCall, cancel: &CancellationToken) -> Option<String> {
        let deps = &self.deps;
        let started = Instant::now();
        let result = self.bounded_search(call, cancel).await?;
        Some(match result {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-success
            Ok(chunks) => {
                if let Err(error) = deps
                    .progress
                    .record_tool_done(self.tenant_id, self.turn_id, ToolKind::SearchKnowledge)
                    .await
                {
                    tracing::warn!(%error, "the knowledge search counter could not be written");
                }
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-knowledge
                deps.metrics.knowledge_search("ok");
                deps.metrics.knowledge_search_latency(started.elapsed());
                deps.metrics.knowledge_search_chunks(chunks.len());
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-knowledge
                if chunks.is_empty() {
                    return Some("no results".to_owned());
                }
                chunks
                    .iter()
                    .map(|chunk| {
                        chunk
                            .text
                            .chars()
                            .take(deps.settings.max_chunk_chars)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-success
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-failure
            Err(failure) => {
                tracing::warn!(?failure, "the knowledge search failed");
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-knowledge
                deps.metrics.knowledge_search("error");
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-knowledge
                "the knowledge search is unavailable".to_owned()
            } // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-failure
        })
    }
}

/// The item that carries `output` as the answer of `call` in the provider input:
/// `{"type": "function_call_output", "call_id", "output"}` for [`ToolResultFormat::FunctionCallOutput`]
/// and `{"type": "tool_result", "tool_use_id", "content"}` for [`ToolResultFormat::ToolResult`].
#[must_use]
pub fn tool_result_item(format: ToolResultFormat, call: &ToolCall, output: &str) -> Value {
    match format {
        ToolResultFormat::FunctionCallOutput => json!({
            "type": "function_call_output", "call_id": call.call_id, "output": output
        }),
        ToolResultFormat::ToolResult => json!({
            "type": "tool_result", "tool_use_id": call.call_id, "content": output
        }),
    }
}

#[cfg(test)]
#[path = "knowledge_tests.rs"]
mod knowledge_tests;
