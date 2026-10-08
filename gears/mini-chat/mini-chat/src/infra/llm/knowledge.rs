//! Knowledge search (DESIGN section 4 "Knowledge Search"): the
//! `search_knowledge` function tool, its arguments and output, and the Azure
//! `OpenAI` knowledge retriever (`POST
//! /{alias}/openai/vector_stores/{vector_store_id}/search?api-version={ver}`
//! through OAGW with the gear's S2S identity). Wired only when
//! `knowledge_search.enabled`.

use std::sync::Arc;

use async_trait::async_trait;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::{Value, json};

use super::ServiceIdentity;
use super::providers::{check_status, gateway_error};
use super::types::{ProviderError, ToolSpec};

/// Name of the knowledge-search function tool.
pub const SEARCH_KNOWLEDGE: &str = "search_knowledge";

/// Tool output once `knowledge_search.max_calls_per_message` retrievals ran.
pub const SEARCH_LIMIT_REACHED: &str =
    "Search limit reached for this message. Answer with the information you already have.";
/// Tool output of a failed retrieval.
pub const SEARCH_FAILED: &str =
    "The knowledge search failed. Answer with the information you already have.";
/// Tool output for arguments without a usable `query`.
pub const INVALID_ARGUMENTS: &str =
    "Invalid search_knowledge arguments: a non-empty \"query\" string is required.";

/// Where the knowledge base lives for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeTarget {
    /// OAGW alias of the `knowledge_search.provider_id` entry (tenant aware).
    pub alias: String,
    pub api_version: String,
    pub vector_store_id: String,
}

/// One retrieved chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeChunk {
    pub filename: String,
    pub score: Option<f64>,
    pub text: String,
}

/// Knowledge retriever port.
#[async_trait]
pub trait KnowledgeRetriever: Send + Sync {
    /// Up to `top_k` chunks for `query`.
    async fn search(
        &self,
        target: &KnowledgeTarget,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<KnowledgeChunk>, ProviderError>;
}

/// Azure `OpenAI` vector store search through OAGW.
pub struct AzureKnowledgeRetriever {
    gw: Arc<dyn ServiceGatewayClientV1>,
    identity: Arc<ServiceIdentity>,
}

impl AzureKnowledgeRetriever {
    #[must_use]
    pub fn new(gw: Arc<dyn ServiceGatewayClientV1>, identity: Arc<ServiceIdentity>) -> Self {
        Self { gw, identity }
    }
}

#[async_trait]
impl KnowledgeRetriever for AzureKnowledgeRetriever {
    async fn search(
        &self,
        target: &KnowledgeTarget,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<KnowledgeChunk>, ProviderError> {
        let ctx = self
            .identity
            .get()
            .await
            .map_err(|_| ProviderError::provider("service identity not ready"))?;
        let body = json!({"query": query, "max_num_results": top_k});
        let uri = format!(
            "/{}/openai/vector_stores/{}/search?api-version={}",
            target.alias, target.vector_store_id, target.api_version
        );
        let req = http::Request::builder()
            .method(http::Method::POST)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .map_err(|e| ProviderError::provider(format!("invalid knowledge request: {e}")))?;
        let resp = match self.gw.proxy_request(ctx, req).await {
            Ok(resp) => check_status(resp).await?,
            Err(e) => return Err(gateway_error(&e)),
        };
        let bytes = resp.into_body().into_bytes().await.map_err(|e| {
            tracing::warn!(error = %e, "knowledge search response body read failed");
            ProviderError::provider("knowledge search response could not be read")
        })?;
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|_| ProviderError::provider("invalid knowledge search response"))?;
        Ok(parse_results(&v))
    }
}

/// The `data` items of a vector store search page; the text parts of each
/// item are concatenated.
fn parse_results(v: &Value) -> Vec<KnowledgeChunk> {
    let items = v.get("data").and_then(Value::as_array);
    items
        .into_iter()
        .flatten()
        .map(|item| KnowledgeChunk {
            filename: item
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            score: item.get("score").and_then(Value::as_f64),
            text: item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect(),
        })
        .collect()
}

/// The `search_knowledge` function tool.
#[must_use]
pub fn search_knowledge_tool() -> ToolSpec {
    ToolSpec::Function {
        name: SEARCH_KNOWLEDGE.to_owned(),
        description:
            "Search the organization's shared knowledge base and return the most relevant excerpts."
                .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to search for."},
                "top_k": {"type": "integer", "description": "Maximum number of excerpts to return.", "minimum": 1}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    }
}

/// Parsed `search_knowledge` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchArgs {
    pub query: String,
    /// Between 1 and `knowledge_search.top_k`.
    pub top_k: usize,
}

/// `query` (required, non-blank) and `top_k` (default and cap `max_top_k`,
/// at least 1). `None` when the arguments are unusable.
#[must_use]
pub fn parse_args(arguments: &str, max_top_k: usize) -> Option<SearchArgs> {
    let v: Value = serde_json::from_str(arguments).ok()?;
    let query = v.get("query")?.as_str()?.trim();
    if query.is_empty() {
        return None;
    }
    let top_k = v
        .get("top_k")
        .and_then(Value::as_u64)
        .map_or(max_top_k, |k| usize::try_from(k).unwrap_or(usize::MAX))
        .clamp(1, max_top_k.max(1));
    Some(SearchArgs {
        query: query.to_owned(),
        top_k,
    })
}

/// The tool output: `{"results": [{filename, score, text}]}` with each text
/// trimmed to `max_chunk_chars` characters.
#[must_use]
pub fn format_output(chunks: &[KnowledgeChunk], max_chunk_chars: usize) -> String {
    let results: Vec<Value> = chunks
        .iter()
        .map(|c| {
            json!({
                "filename": c.filename,
                "score": c.score,
                "text": c.text.chars().take(max_chunk_chars).collect::<String>(),
            })
        })
        .collect();
    json!({ "results": results }).to_string()
}

#[cfg(test)]
#[path = "knowledge_tests.rs"]
mod knowledge_tests;
