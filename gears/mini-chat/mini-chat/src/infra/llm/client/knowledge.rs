//! Azure OpenAI knowledge retriever over OAGW (DESIGN §4 "Knowledge Search"):
//! `POST /{alias}/openai/vector_stores/{vector_store_id}/search?api-version={ver}`.

use async_trait::async_trait;
use http::Method;
use oagw_sdk::Body;
use serde_json::{Value, json};
use uuid::Uuid;

use super::super::sanitize::sanitize_provider_message;
use super::super::{KnowledgeChunk, KnowledgeRetriever, StorageError};
use super::OagwProviderClient;
use super::errors::{read_body, storage_error_message};

/// Request URI of a knowledge search.
pub(crate) fn search_uri(alias: &str, vector_store_id: &str, api_version: &str) -> String {
    format!("/{alias}/openai/vector_stores/{vector_store_id}/search?api-version={api_version}")
}

/// Chunks of a vector store search response (`data[*].content[*].text` joined per result).
pub(crate) fn parse_search_results(v: &Value) -> Vec<KnowledgeChunk> {
    v.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let text = match r.get("content") {
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(Value::String(t)) => t.clone(),
                _ => r.get("text").and_then(Value::as_str).unwrap_or("").to_owned(),
            };
            if text.trim().is_empty() {
                return None;
            }
            Some(KnowledgeChunk {
                text,
                filename: r.get("filename").and_then(Value::as_str).map(str::to_owned),
                score: r.get("score").and_then(Value::as_f64),
            })
        })
        .collect()
}

#[async_trait]
impl KnowledgeRetriever for OagwProviderClient {
    async fn search(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
        query: &str,
        max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, StorageError> {
        let rp = self
            .resolver
            .resolve(provider_id, tenant_id)
            .ok_or_else(|| StorageError::Config(format!("unknown knowledge provider '{provider_id}'")))?;
        let api_version = rp
            .api_version
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| StorageError::Config("knowledge provider has no api_version".to_owned()))?;
        let ctx = self.s2s.get().await.map_err(StorageError::Transport)?;
        let body = serde_json::to_vec(&json!({
            "query": query,
            "max_num_results": max_num_results,
        }))
        .map_err(|e| StorageError::Config(format!("serialize knowledge search: {e}")))?;
        let req = http::Request::builder()
            .method(Method::POST)
            .uri(search_uri(&rp.alias, vector_store_id, api_version))
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .map_err(|e| StorageError::Config(format!("invalid knowledge search request: {e}")))?;
        let resp = self.oagw.proxy_request(ctx, req).await.map_err(|e| {
            tracing::warn!(error = %e.detail(), "OAGW knowledge search call failed");
            StorageError::Transport(sanitize_provider_message(e.detail()))
        })?;
        let status = resp.status();
        let bytes = read_body(resp.into_body()).await;
        if !status.is_success() {
            return Err(StorageError::Http {
                status: status.as_u16(),
                message: storage_error_message(status, &bytes),
            });
        }
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(parse_search_results(&v))
    }
}
