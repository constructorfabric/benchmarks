//! Knowledge search (DESIGN §3.2 "Knowledge retriever", §4 "Knowledge Search"):
//! the retriever port and its Azure `OpenAI` implementation, plus the per-request
//! parameters of the `search_knowledge` agentic loop.
//!
//! The retriever calls `POST /{alias}/openai/vector_stores/{vector_store_id}/search
//! ?api-version={ver}` with `{query, max_num_results}` through OAGW (S2S
//! context); each result's text parts form one chunk, trimmed to
//! `knowledge_search.max_chunk_chars` characters.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use http::Method;
use oagw_sdk::Body;
use serde_json::{Value, json};
use tracing::warn;
use uuid::Uuid;

use super::storage::request;
use super::{LlmError, ProviderResolver, RagClient, ResolvedProvider, StorageError};
use crate::config::{KnowledgeSearchConfig, ProviderKind};

/// Knowledge base search port.
#[async_trait]
pub trait KnowledgeRetriever: Send + Sync {
    /// Up to `top_k` text chunks relevant to `query`.
    ///
    /// # Errors
    /// The search could not be run or the provider failed.
    async fn search(&self, query: &str, top_k: usize) -> Result<Vec<String>, LlmError>;
}

/// Azure `OpenAI` vector-store search.
pub struct AzureKnowledgeRetriever {
    rag: Arc<RagClient>,
    provider: ResolvedProvider,
    vector_store_id: String,
    max_chunk_chars: usize,
}

impl AzureKnowledgeRetriever {
    #[must_use]
    pub fn new(
        rag: Arc<RagClient>,
        provider: ResolvedProvider,
        vector_store_id: String,
        max_chunk_chars: usize,
    ) -> Self {
        Self {
            rag,
            provider,
            vector_store_id,
            max_chunk_chars,
        }
    }
}

/// `/{alias}/openai/vector_stores/{vs}/search?api-version={ver}`.
fn search_uri(p: &ResolvedProvider, vector_store_id: &str) -> String {
    format!(
        "/{}/openai/vector_stores/{vector_store_id}/search?api-version={}",
        p.alias,
        p.api_version.as_deref().unwrap_or_default()
    )
}

/// Text of one search result: its `text` content parts joined by a newline.
fn result_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| {
            c.get("type")
                .and_then(Value::as_str)
                .is_none_or(|t| t == "text")
        })
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn llm_error(e: StorageError) -> LlmError {
    match e {
        StorageError::Transient(m) => LlmError::Unavailable(m),
        StorageError::Failed(message) => LlmError::Provider { message },
    }
}

#[async_trait]
impl KnowledgeRetriever for AzureKnowledgeRetriever {
    async fn search(&self, query: &str, top_k: usize) -> Result<Vec<String>, LlmError> {
        let body = json!({"query": query, "max_num_results": top_k});
        let resp = self
            .rag
            .call(&self.provider, |p| {
                request(
                    Method::POST,
                    &search_uri(p, &self.vector_store_id),
                    Body::from(body.to_string()),
                    Some("application/json"),
                )
            })
            .await
            .map_err(llm_error)?;
        Ok(resp
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|r| {
                result_text(r)
                    .chars()
                    .take(self.max_chunk_chars)
                    .collect::<String>()
            })
            .filter(|chunk| !chunk.is_empty())
            .collect())
    }
}

/// Knowledge-search parameters of one turn.
#[derive(Clone)]
pub struct KnowledgeTurn {
    pub retriever: Arc<dyn KnowledgeRetriever>,
    /// `knowledge_search.max_calls_per_message`.
    pub max_calls: u32,
    /// `knowledge_search.top_k`: cap of the model's `top_k` (and its default).
    pub top_k: usize,
}

impl fmt::Debug for KnowledgeTurn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KnowledgeTurn")
            .field("max_calls", &self.max_calls)
            .field("top_k", &self.top_k)
            .finish_non_exhaustive()
    }
}

/// Builds the per-request knowledge-search parameters (created at gear
/// initialization only when `knowledge_search.enabled`).
pub struct KnowledgeSearch {
    cfg: KnowledgeSearchConfig,
    providers: Arc<ProviderResolver>,
    rag: Arc<RagClient>,
}

impl KnowledgeSearch {
    #[must_use]
    pub fn new(
        cfg: &KnowledgeSearchConfig,
        providers: Arc<ProviderResolver>,
        rag: Arc<RagClient>,
    ) -> Self {
        Self {
            cfg: cfg.clone(),
            providers,
            rag,
        }
    }

    /// Parameters for a request of `tenant_id`, or `None` (knowledge search off
    /// for the request, with a warning) when the provider named by
    /// `knowledge_search.provider_id` is not configured, is not of kind
    /// `openai_responses` / `anthropic_messages`, or has no alias or
    /// `api_version` for the tenant.
    #[must_use]
    pub fn for_tenant(&self, tenant_id: Uuid) -> Option<KnowledgeTurn> {
        if !self.cfg.enabled {
            return None;
        }
        let vector_store_id = self.cfg.vector_store_id.clone()?;
        let provider = match self.usable_provider(tenant_id) {
            Ok(p) => p,
            Err(reason) => {
                warn!(%tenant_id, reason, "knowledge search off for the request");
                return None;
            }
        };
        Some(KnowledgeTurn {
            retriever: Arc::new(AzureKnowledgeRetriever::new(
                Arc::clone(&self.rag),
                provider,
                vector_store_id,
                self.cfg.max_chunk_chars,
            )),
            max_calls: self.cfg.max_calls_per_message,
            top_k: self.cfg.top_k,
        })
    }

    /// The knowledge provider for `tenant_id`, or why it cannot be used.
    fn usable_provider(&self, tenant_id: Uuid) -> Result<ResolvedProvider, String> {
        let provider_id = self
            .cfg
            .provider_id
            .as_deref()
            .ok_or("knowledge_search.provider_id is not set")?;
        let provider = self
            .providers
            .resolve(provider_id, tenant_id)
            .map_err(|e| e.to_string())?;
        if !matches!(
            provider.kind,
            ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages
        ) {
            return Err(format!(
                "provider '{provider_id}' is of kind {:?}, not openai_responses or anthropic_messages",
                provider.kind
            ));
        }
        if provider.alias.trim().is_empty() || provider.api_version.is_none() {
            return Err(format!(
                "provider '{provider_id}' has no upstream alias or api_version"
            ));
        }
        Ok(provider)
    }
}

#[cfg(test)]
#[path = "knowledge_tests.rs"]
mod knowledge_tests;
