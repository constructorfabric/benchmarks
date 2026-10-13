//! The knowledge retriever over the OAGW in-process proxy
//! (`cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`, `inst-retrieve`):
//! one vector store search of the RAG provider,
//! `POST /{alias}/openai/vector_stores/{vector_store_id}/search?api-version={ver}`.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::{
    KnowledgeChunk, KnowledgeRetriever, KnowledgeSearchRequest, ProviderDirectory,
};

/// A search page larger than this is a failed retrieval.
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

/// Retrieves chunks from the knowledge vector store through OAGW.
pub struct OagwKnowledgeRetriever {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    providers: Arc<dyn ProviderDirectory>,
    provider_id: String,
    vector_store_id: String,
}

impl OagwKnowledgeRetriever {
    /// The retriever of `knowledge_search.provider_id` and
    /// `knowledge_search.vector_store_id`; the alias of the tenant and the
    /// `api_version` are resolved through `providers` on every search.
    #[must_use]
    pub fn new(
        oagw: Arc<dyn ServiceGatewayClientV1>,
        providers: Arc<dyn ProviderDirectory>,
        provider_id: impl Into<String>,
        vector_store_id: impl Into<String>,
    ) -> Self {
        Self {
            oagw,
            providers,
            provider_id: provider_id.into(),
            vector_store_id: vector_store_id.into(),
        }
    }
}

fn internal(message: &str) -> DomainError {
    DomainError::Internal(message.to_owned())
}

/// The chunks of a search page: one per result with text, its text parts
/// joined by a newline.
fn parse_page(page: &Value) -> Result<Vec<KnowledgeChunk>, DomainError> {
    let data = page
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| internal("the search page has no data array"))?;
    Ok(data
        .iter()
        .filter_map(|result| {
            let parts: Vec<&str> = result["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect();
            (!parts.is_empty()).then(|| KnowledgeChunk {
                text: parts.join("\n"),
            })
        })
        .collect())
}

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-retrieve
#[async_trait]
impl KnowledgeRetriever for OagwKnowledgeRetriever {
    async fn search(
        &self,
        ctx: &SecurityContext,
        request: &KnowledgeSearchRequest<'_>,
    ) -> Result<Vec<KnowledgeChunk>, DomainError> {
        let target = self
            .providers
            .resolve(&self.provider_id, request.tenant_id)?;
        let version = target
            .api_version
            .as_deref()
            .filter(|version| !version.is_empty())
            .ok_or_else(|| internal("the knowledge provider has no api_version"))?;
        let uri = format!(
            "/{}/openai/vector_stores/{}/search?api-version={version}",
            target.alias, self.vector_store_id
        );
        let payload = serde_json::to_vec(&json!({
            "query": request.query,
            "max_num_results": request.top_k,
        }))
        .map_err(|_| internal("the search body cannot be serialized"))?;
        let http_request = http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(Body::from(payload))
            .map_err(|_| internal("the search request cannot be built"))?;
        let response = self
            .oagw
            .proxy_request(ctx.clone(), http_request)
            .await
            .map_err(|error| DomainError::Internal(format!("the search call failed: {error}")))?;
        let (parts, body) = response.into_parts();
        if !parts.status.is_success() {
            return Err(DomainError::Internal(format!(
                "the search answered with status {}",
                parts.status.as_u16()
            )));
        }
        let mut stream = body.into_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| internal("the search body could not be read"))?;
            if bytes.len() + chunk.len() > RESPONSE_LIMIT {
                return Err(internal("the search page is too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let page: Value =
            serde_json::from_slice(&bytes).map_err(|_| internal("the search page is not JSON"))?;
        parse_page(&page)
    }
}

// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-knowledge-search:p1:inst-retrieve
#[cfg(test)]
#[path = "knowledge_retriever_tests.rs"]
mod tests;
