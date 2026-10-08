//! Azure `OpenAI` knowledge retriever (D§4 "Knowledge Search"):
//! `POST /{alias}/openai/vector_stores/{vector_store_id}/search?api-version=…`.

use std::sync::Arc;

use async_trait::async_trait;
use http::Method;
use oagw_sdk::ServiceGatewayClientV1;
use serde_json::{Value, json};

use super::{Payload, RagStorage, path_id};
use crate::domain::error::DomainError;
use crate::domain::ports::{KnowledgeChunk, KnowledgeRetriever};
use crate::infra::llm::provider_resolver::StorageTarget;
use crate::infra::s2s::S2sContextProvider;

/// Searches one Azure `OpenAI` vector store.
pub struct AzureKnowledgeRetriever {
    storage: RagStorage,
    /// Azure target (`storage_kind = azure`, the entry's alias and
    /// `api_version`).
    target: StorageTarget,
    vector_store_id: String,
}

impl AzureKnowledgeRetriever {
    #[must_use]
    pub fn new(
        oagw: Arc<dyn ServiceGatewayClientV1>,
        s2s: Arc<S2sContextProvider>,
        target: StorageTarget,
        vector_store_id: String,
    ) -> Self {
        Self {
            storage: RagStorage::new(oagw, s2s),
            target,
            vector_store_id,
        }
    }
}

#[async_trait]
impl KnowledgeRetriever for AzureKnowledgeRetriever {
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<KnowledgeChunk>, DomainError> {
        let vs = path_id("vector store", &self.vector_store_id)
            .map_err(|e| DomainError::Internal(e.to_string()))?;
        let json = self
            .storage
            .call(
                "knowledge search",
                &self.target,
                Method::POST,
                &format!("/vector_stores/{vs}/search"),
                Payload::Json(json!({"query": query, "max_num_results": top_k})),
                false,
            )
            .await
            .map_err(|e| DomainError::Internal(e.to_string()))?;
        Ok(json["data"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| KnowledgeChunk {
                filename: r["filename"].as_str().unwrap_or_default().to_owned(),
                score: r["score"].as_f64().unwrap_or(0.0),
                text: r["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
            })
            .collect())
    }
}
