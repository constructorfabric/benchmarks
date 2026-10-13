//! Azure `OpenAI` knowledge retriever (DESIGN section 4 "Knowledge Search",
//! section 3.5 "LLM Provider" RAG routes, Appendix B.7).
//!
//! Created at gear initialization only when `knowledge_search.enabled =
//! true`. A search is `POST /{alias}/openai/vector_stores/{vector_store_id}
//! /search?api-version={api_version}` with `{"query": ..., "max_num_results":
//! n}`, sent through the in-process OAGW client under the gear's S2S security
//! context (the upstream and its RAG routes are provisioned for the
//! `knowledge_search.provider_id` entry). The alias is the tenant override's
//! alias when the tenant has one, else the entry's `upstream_alias`.
//!
//! The response's `data[]` become [`KnowledgeChunk`]s: the `text` parts of
//! `content` joined, the `filename` and the `score`. Any gateway failure, a
//! non-2xx status or a body that is not a search result page is a
//! [`KnowledgeSearchError`] (diagnostic only).
//!
//! Metrics: `{prefix}_knowledge_search{result = ok | error}`,
//! `{prefix}_knowledge_search_latency_ms` and
//! `{prefix}_knowledge_search_chunks` (chunks of a successful search).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use oagw_sdk::api::ServiceGatewayClientV1;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use serde_json::{Value, json};
use uuid::Uuid;

use super::openai_responses::encode_path_segment;
use super::resolver::tenant_alias;
use super::storage::{Expect, classify};
use super::transport::build_http_request;
use crate::config::{KnowledgeSearchConfig, ProviderEntry, ProviderKind};
use crate::domain::knowledge::{KnowledgeChunk, KnowledgeRetriever, KnowledgeSearchError};
use crate::infra::oagw_provisioning::SystemContextSource;

/// Knowledge-search metrics.
struct KnowledgeMetrics {
    searches: Counter<u64>,
    latency_ms: Histogram<f64>,
    chunks: Histogram<u64>,
}

impl KnowledgeMetrics {
    fn new(prefix: &str) -> Self {
        let scope = opentelemetry::InstrumentationScope::builder("mini-chat").build();
        let meter = opentelemetry::global::meter_with_scope(scope);
        Self {
            searches: meter
                .u64_counter(format!("{prefix}_knowledge_search"))
                .with_description("Knowledge base searches by result")
                .build(),
            latency_ms: meter
                .f64_histogram(format!("{prefix}_knowledge_search_latency_ms"))
                .with_description("Knowledge base search latency")
                .with_unit("ms")
                .build(),
            chunks: meter
                .u64_histogram(format!("{prefix}_knowledge_search_chunks"))
                .with_description("Chunks returned by a successful knowledge base search")
                .build(),
        }
    }
}

/// The Azure `OpenAI` knowledge retriever (module docs).
pub struct AzureKnowledgeRetriever {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    system: Arc<dyn SystemContextSource>,
    provider_id: String,
    /// The `knowledge_search.provider_id` entry, `None` when not configured.
    entry: Option<ProviderEntry>,
    vector_store_id: String,
    metrics: KnowledgeMetrics,
}

impl std::fmt::Debug for AzureKnowledgeRetriever {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureKnowledgeRetriever")
            .field("provider_id", &self.provider_id)
            .finish_non_exhaustive()
    }
}

impl AzureKnowledgeRetriever {
    /// The retriever of the configuration, or `None` when knowledge search is
    /// disabled. `providers` are the prepared entries (upstream aliases
    /// filled).
    #[must_use]
    pub fn from_config(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        system: Arc<dyn SystemContextSource>,
        config: &KnowledgeSearchConfig,
        providers: &BTreeMap<String, ProviderEntry>,
        metrics_prefix: &str,
    ) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let provider_id = config.provider_id.clone().unwrap_or_default();
        let entry = providers.get(&provider_id).cloned();
        Some(Self {
            gateway,
            system,
            provider_id,
            entry,
            vector_store_id: config.vector_store_id.clone().unwrap_or_default(),
            metrics: KnowledgeMetrics::new(metrics_prefix),
        })
    }

    /// The proxy URI of a search for `tenant_id`.
    ///
    /// # Errors
    ///
    /// The reason when the parameters cannot be built (see
    /// [`KnowledgeRetriever::check`]).
    pub fn search_uri(&self, tenant_id: Uuid) -> Result<String, String> {
        let provider_id = &self.provider_id;
        let entry = self.entry.as_ref().ok_or_else(|| {
            format!("knowledge_search.provider_id '{provider_id}' is not a configured provider")
        })?;
        if !matches!(
            entry.kind,
            ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages
        ) {
            return Err(format!(
                "provider '{provider_id}' must be of kind openai_responses or anthropic_messages"
            ));
        }
        let alias = tenant_alias(entry, tenant_id).trim();
        if alias.is_empty() {
            return Err(format!(
                "provider '{provider_id}' has no upstream alias for the tenant"
            ));
        }
        let version = entry
            .api_version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .ok_or_else(|| format!("provider '{provider_id}' has no api_version"))?;
        if self.vector_store_id.trim().is_empty() {
            return Err("knowledge_search.vector_store_id is empty".to_owned());
        }
        Ok(format!(
            "/{alias}/openai/vector_stores/{}/search?api-version={version}",
            encode_path_segment(&self.vector_store_id)
        ))
    }

    async fn run_search(
        &self,
        uri: &str,
        query: &str,
        max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, KnowledgeSearchError> {
        let body = serde_json::to_vec(&json!({
            "query": query,
            "max_num_results": max_num_results,
        }))
        .map_err(|err| KnowledgeSearchError(format!("request body: {err}")))?;
        let request = build_http_request(uri, body, false, &[])
            .map_err(|_| KnowledgeSearchError("request could not be built".to_owned()))?;
        let ctx = self
            .system
            .system_context()
            .await
            .map_err(|err| KnowledgeSearchError(format!("S2S context unavailable: {err}")))?;
        let response = self
            .gateway
            .proxy_request(ctx, request)
            .await
            .map_err(|err| KnowledgeSearchError(format!("gateway failure: {err}")))?;
        let page = classify(response, Expect::Json)
            .await
            .map_err(|err| KnowledgeSearchError(err.to_string()))?;
        let mut chunks = parse_search_page(&page)?;
        chunks.truncate(max_num_results);
        Ok(chunks)
    }
}

#[async_trait]
impl KnowledgeRetriever for AzureKnowledgeRetriever {
    fn check(&self, tenant_id: Uuid) -> Result<(), String> {
        self.search_uri(tenant_id).map(|_| ())
    }

    async fn search(
        &self,
        tenant_id: Uuid,
        query: &str,
        max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, KnowledgeSearchError> {
        let uri = self.search_uri(tenant_id).map_err(KnowledgeSearchError)?;
        let started = Instant::now();
        let result = self.run_search(&uri, query, max_num_results).await;
        self.metrics
            .latency_ms
            .record(started.elapsed().as_secs_f64() * 1000.0, &[]);
        let outcome = match &result {
            Ok(chunks) => {
                self.metrics
                    .chunks
                    .record(u64::try_from(chunks.len()).unwrap_or(u64::MAX), &[]);
                "ok"
            }
            Err(err) => {
                tracing::debug!(
                    target: "mini_chat.knowledge",
                    provider = %self.provider_id,
                    error = %err,
                    "knowledge search failed"
                );
                "error"
            }
        };
        self.metrics
            .searches
            .add(1, &[KeyValue::new("result", outcome)]);
        result
    }
}

/// The chunks of a vector store search result page (`data[]`).
///
/// # Errors
///
/// [`KnowledgeSearchError`] when the body has no `data` array.
pub fn parse_search_page(page: &Value) -> Result<Vec<KnowledgeChunk>, KnowledgeSearchError> {
    let data = page
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| KnowledgeSearchError("search response has no data array".to_owned()))?;
    Ok(data
        .iter()
        .filter_map(|result| {
            let text: String = result
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|part| {
                    part.get("type")
                        .and_then(Value::as_str)
                        .is_none_or(|t| t == "text")
                })
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<&str>>()
                .join("\n");
            (!text.is_empty()).then(|| KnowledgeChunk {
                text,
                filename: result
                    .get("filename")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
                score: result.get("score").and_then(Value::as_f64),
            })
        })
        .collect())
}

#[cfg(test)]
#[path = "knowledge_tests.rs"]
mod tests;
