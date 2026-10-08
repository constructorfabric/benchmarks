//! Knowledge retriever (DESIGN "Knowledge Search"): the organization knowledge base is one
//! Azure `OpenAI` vector store, searched through OAGW with the S2S context at
//! `POST /{alias}/openai/vector_stores/{vector_store_id}/search?api-version={ver}`.

use std::sync::Arc;

use async_trait::async_trait;
use http::Method;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::{Value, json};

use super::StorageError;
use super::openai::{invalid_request, json_body, path_id, proxy};
use crate::infra::llm::S2sContext;

/// Where the knowledge base of a tenant is searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeTarget {
    pub alias: String,
    pub api_version: String,
}

/// One retrieved passage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeChunk {
    /// Filename of the source document (may be empty).
    pub source: String,
    pub text: String,
}

/// Searches the organization knowledge base.
#[async_trait]
pub trait KnowledgeRetriever: Send + Sync {
    /// At most `max_num_results` passages for `query` from vector store `vector_store_id`.
    async fn search(
        &self,
        target: &KnowledgeTarget,
        vector_store_id: &str,
        query: &str,
        max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, StorageError>;
}

/// Azure `OpenAI` vector store search; every passage is trimmed to `max_chunk_chars`
/// characters.
pub struct AzureKnowledgeRetriever {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: S2sContext,
    max_chunk_chars: usize,
}

impl AzureKnowledgeRetriever {
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        s2s: S2sContext,
        max_chunk_chars: usize,
    ) -> Self {
        Self {
            gateway,
            s2s,
            max_chunk_chars,
        }
    }
}

#[async_trait]
impl KnowledgeRetriever for AzureKnowledgeRetriever {
    async fn search(
        &self,
        target: &KnowledgeTarget,
        vector_store_id: &str,
        query: &str,
        max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, StorageError> {
        let uri = format!(
            "/{}/openai/vector_stores/{}/search?api-version={}",
            target.alias,
            path_id(vector_store_id)?,
            target.api_version
        );
        let body = serde_json::to_vec(&json!({"query": query, "max_num_results": max_num_results}))
            .map_err(|_| invalid_request("failed to encode the search request"))?;
        let req = http::Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "application/json")
            .body(Body::from(body))
            .map_err(|_| invalid_request("failed to build the search request"))?;
        let reply = proxy(&self.gateway, &self.s2s, req).await?;
        let page = json_body(&reply.into_success()?);
        Ok(page
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|hit| self.chunk(hit))
            .collect())
    }
}

impl AzureKnowledgeRetriever {
    /// The text parts of one search hit joined with newlines, trimmed to `max_chunk_chars`.
    fn chunk(&self, hit: &Value) -> KnowledgeChunk {
        let text = hit
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        KnowledgeChunk {
            source: hit
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            text: text.chars().take(self.max_chunk_chars).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use http::Method;
    use serde_json::json;

    use super::*;
    use crate::test_support::authn::s2s_security_context;
    use crate::test_support::gateway::{FakeGateway, Responder};

    fn setup(max_chunk_chars: usize) -> (Arc<FakeGateway>, AzureKnowledgeRetriever) {
        let gateway = Arc::new(FakeGateway::new());
        let s2s = S2sContext::new();
        s2s.set(s2s_security_context());
        let retriever =
            AzureKnowledgeRetriever::new(Arc::clone(&gateway) as _, s2s, max_chunk_chars);
        (gateway, retriever)
    }

    fn target() -> KnowledgeTarget {
        KnowledgeTarget {
            alias: "azure.test".to_owned(),
            api_version: "2025-03-01-preview".to_owned(),
        }
    }

    #[tokio::test]
    async fn knowledge_search_loop_retriever_request_and_trimming() {
        let (gateway, retriever) = setup(5);
        gateway.on(
            Method::POST,
            "/azure.test/openai/vector_stores/vs_kb/search",
            Responder::json(
                200,
                json!({"object": "vector_store.search_results.page", "data": [
                    {"file_id": "file-abcdefghijklmnop", "filename": "handbook.pdf", "score": 0.9,
                     "content": [{"type": "text", "text": "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{43c}\u{438}\u{440}"}, {"type": "text", "text": "more"}]},
                    {"file_id": "file-x", "filename": "b.txt", "content": [{"type": "text", "text": "abc"}]},
                ], "has_more": false}),
            ),
        );

        let chunks = retriever
            .search(&target(), "vs_kb", "vacation policy", 3)
            .await
            .expect("search");

        assert_eq!(
            chunks,
            vec![
                KnowledgeChunk {
                    source: "handbook.pdf".to_owned(),
                    text: "\u{41f}\u{440}\u{438}\u{432}\u{435}".to_owned(),
                },
                KnowledgeChunk {
                    source: "b.txt".to_owned(),
                    text: "abc".to_owned(),
                },
            ]
        );
        let recorded = gateway.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].uri,
            "/azure.test/openai/vector_stores/vs_kb/search?api-version=2025-03-01-preview"
        );
        assert_eq!(
            recorded[0].json,
            Some(json!({"query": "vacation policy", "max_num_results": 3}))
        );

        // provider errors are storage errors
        let (gateway, retriever) = setup(5);
        gateway.on(
            Method::POST,
            "/search",
            Responder::json(500, json!({"error": {"message": "down"}})),
        );
        assert!(matches!(
            retriever.search(&target(), "vs_kb", "q", 3).await,
            Err(StorageError::Transient(_))
        ));
    }
}
