//! The knowledge-search loop
//! (`cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`): the
//! [`ToolUseHandler`] that answers `search_knowledge` calls through the
//! knowledge retriever and issues the next provider request.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::UsageTokens;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::ports::{
    KnowledgeRetriever, KnowledgeSearchRequest, ProviderClient, StreamMetrics, ToolCallOutput,
};
use super::task::{ToolUseContext, ToolUseHandler, ToolUseOutcome};
use super::tools::KnowledgeParams;
use super::types::{FunctionCall, ProviderFailure, StreamErrorCode};

/// The only function the loop answers.
const SEARCH_KNOWLEDGE: &str = "search_knowledge";
/// The answer of a call after the per-message retrieval limit.
const LIMIT_OUTPUT: &str = "{\"error\":\"search limit reached\"}";
/// The answer of a call whose retrieval failed.
const FAILURE_OUTPUT: &str = "{\"error\":\"search failed\"}";
/// The answer of a call whose arguments hold no query.
const INVALID_OUTPUT: &str = "{\"error\":\"invalid arguments\"}";

/// Answers the `search_knowledge` calls of a turn.
pub struct KnowledgeToolUse {
    client: Arc<dyn ProviderClient>,
    retriever: Arc<dyn KnowledgeRetriever>,
    metrics: Arc<dyn StreamMetrics>,
}

impl KnowledgeToolUse {
    /// The handler over the provider client (continuation and next request),
    /// the retriever and the metrics sink.
    #[must_use]
    pub fn new(
        client: Arc<dyn ProviderClient>,
        retriever: Arc<dyn KnowledgeRetriever>,
        metrics: Arc<dyn StreamMetrics>,
    ) -> Self {
        Self {
            client,
            retriever,
            metrics,
        }
    }

    /// The function output of one call; counts the call before anything runs.
    async fn answer(
        &self,
        context: &ToolUseContext<'_>,
        params: &KnowledgeParams,
        call: &FunctionCall,
    ) -> String {
        let counters = context.knowledge;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-count-call
        counters.record_call();
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-count-call
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-if-limit-output
        if counters.retrievals() >= params.max_calls_per_message {
            return LIMIT_OUTPUT.to_owned();
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-if-limit-output
        let Some((query, top_k)) = parse_arguments(&call.arguments, params.top_k) else {
            return INVALID_OUTPUT.to_owned();
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-retrieve
        counters.record_retrieval();
        let started = Instant::now();
        let result = self
            .retriever
            .search(
                &context.run.ctx,
                &KnowledgeSearchRequest {
                    tenant_id: context.run.chat.tenant_id,
                    query: &query,
                    top_k,
                },
            )
            .await;
        let output = match result {
            Ok(chunks) => {
                counters.record_completed();
                self.metrics.knowledge_search("ok");
                self.metrics
                    .knowledge_search_latency_ms(started.elapsed().as_secs_f64() * 1000.0);
                self.metrics
                    .knowledge_search_chunks(u64::try_from(chunks.len()).unwrap_or(u64::MAX));
                let items: Vec<Value> = chunks
                    .iter()
                    .map(|chunk| {
                        json!({ "text": chunk.text.chars().take(params.max_chunk_chars).collect::<String>() })
                    })
                    .collect();
                json!({ "results": items }).to_string()
            }
            Err(error) => {
                tracing::warn!(%error, "a knowledge retrieval failed, the call is answered with a failure");
                self.metrics.knowledge_search("error");
                FAILURE_OUTPUT.to_owned()
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-retrieve
        output
    }
}

/// The query and the capped `top_k` of a call; `None` without a query.
fn parse_arguments(arguments: &str, cap: usize) -> Option<(String, usize)> {
    let json: Value = serde_json::from_str(arguments).ok()?;
    let query = json.get("query")?.as_str()?;
    if query.trim().is_empty() {
        return None;
    }
    let cap = cap.max(1);
    let top_k = json
        .get("top_k")
        .and_then(Value::as_u64)
        .map_or(cap, |requested| {
            usize::try_from(requested)
                .unwrap_or(usize::MAX)
                .clamp(1, cap)
        });
    Some((query.to_owned(), top_k))
}

#[async_trait]
impl ToolUseHandler for KnowledgeToolUse {
    async fn handle(
        &self,
        context: &ToolUseContext<'_>,
        calls: &[FunctionCall],
        _usage: Option<UsageTokens>,
    ) -> ToolUseOutcome {
        let unexpected =
            || ToolUseOutcome::Fail(ProviderFailure::generic(StreamErrorCode::UnexpectedToolUse));
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-if-unexpected
        let Some(params) = context
            .run
            .tools
            .knowledge
            .as_ref()
            .filter(|_| calls.iter().all(|call| call.name == SEARCH_KNOWLEDGE))
        else {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-return-unexpected
            return unexpected();
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-return-unexpected
        };
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-if-unexpected
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-if-iterations
        if context.iteration >= params.max_calls_per_message.saturating_add(2) {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-return-iterations
            return ToolUseOutcome::Fail(ProviderFailure::generic(
                StreamErrorCode::AgenticIterationsExceeded,
            ));
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-return-iterations
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-if-iterations
        let mut outputs = Vec::with_capacity(calls.len());
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-for-each-call
        for call in calls {
            let output = self.answer(context, params, call).await;
            outputs.push(ToolCallOutput {
                call: call.clone(),
                output,
            });
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-for-each-call
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-append-output
        let next = match self.client.continue_request(context.request, &outputs) {
            Ok(next) => next,
            Err(error) => {
                tracing::error!(%error, "the next knowledge-search request cannot be built");
                return ToolUseOutcome::Fail(ProviderFailure::generic(
                    StreamErrorCode::ProviderError,
                ));
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-append-output
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-return-next
        match self.client.open(&context.run.ctx, &next).await {
            Ok(stream) => ToolUseOutcome::Continue {
                stream,
                request: next,
            },
            Err(failure) => ToolUseOutcome::Fail(failure),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-return-next
    }
}

#[cfg(test)]
#[path = "knowledge_tests.rs"]
mod tests;
