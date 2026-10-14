//! The OAGW adapter of the knowledge retriever port: `POST
//! /{alias}/openai/vector_stores/{knowledge_search.vector_store_id}/search` with the `api-version`
//! query of the provider entry named by `knowledge_search.provider_id`, under the gear's S2S
//! security context (no credential; OAGW injects it). The body carries the model's `query` and
//! `max_num_results` (the capped `top_k`); the result chunks are the `text` parts of the `content`
//! of each `data` entry.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use oagw_sdk::Body;
use oagw_sdk::api::ServiceGatewayClientV1;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::KnowledgeSearchConfig;
use crate::domain::attachments::knowledge::{KnowledgeChunk, KnowledgeRetriever, RetrievalFailure};
use crate::domain::provider_registry::ProviderRegistry;
use crate::infra::provider_task::S2sContext;

/// The adapter over the gateway client.
pub struct OagwKnowledgeRetriever {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
    providers: Arc<ProviderRegistry>,
    config: KnowledgeSearchConfig,
}

impl OagwKnowledgeRetriever {
    /// Create the adapter.
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        s2s: Arc<S2sContext>,
        providers: Arc<ProviderRegistry>,
        config: KnowledgeSearchConfig,
    ) -> Self {
        Self {
            gateway,
            s2s,
            providers,
            config,
        }
    }
}

/// The most bytes of a search answer that are read.
const MAX_ANSWER_BYTES: usize = 1 << 20;

/// The longest the body of a search answer may take to arrive: a stalled body is a failed
/// retrieval, never an unbounded wait.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(20);

async fn read_bounded(body: Body) -> Result<Vec<u8>, RetrievalFailure> {
    let mut stream = body.into_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| RetrievalFailure::Transport)?;
        if out.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(RetrievalFailure::Malformed);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[async_trait]
impl KnowledgeRetriever for OagwKnowledgeRetriever {
    async fn search(
        &self,
        tenant_id: Uuid,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<KnowledgeChunk>, RetrievalFailure> {
        let (Some(provider_id), Some(store_id)) =
            (&self.config.provider_id, &self.config.vector_store_id)
        else {
            return Err(RetrievalFailure::Transport);
        };
        let provider = self
            .providers
            .resolve(provider_id, tenant_id)
            .map_err(|_| RetrievalFailure::Transport)?;
        let version = provider
            .api_version
            .clone()
            .filter(|version| !version.is_empty())
            .ok_or(RetrievalFailure::Transport)?;
        let ctx = self.s2s.get().cloned().ok_or(RetrievalFailure::Transport)?;
        let uri = format!(
            "/{}/openai/vector_stores/{store_id}/search?api-version={version}",
            provider.upstream_alias
        );
        let body = json!({ "query": query, "max_num_results": top_k });
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(Bytes::from(body.to_string())))
            .map_err(|_| RetrievalFailure::Transport)?;
        let response = self
            .gateway
            .proxy_request(ctx, request)
            .await
            .map_err(|_| RetrievalFailure::Transport)?;
        let status = response.status().as_u16();
        let bytes = tokio::time::timeout(BODY_READ_TIMEOUT, read_bounded(response.into_body()))
            .await
            .map_err(|_| RetrievalFailure::Timeout)??;
        if !(200..300).contains(&status) {
            return Err(RetrievalFailure::Status(status));
        }
        let page: Value =
            serde_json::from_slice(&bytes).map_err(|_| RetrievalFailure::Malformed)?;
        let results = page
            .get("data")
            .and_then(Value::as_array)
            .ok_or(RetrievalFailure::Malformed)?;
        Ok(results
            .iter()
            .map(|result| {
                let text: Vec<&str> = result
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect()
                    })
                    .unwrap_or_default();
                KnowledgeChunk {
                    text: text.join("\n"),
                }
            })
            .collect())
    }
}

#[cfg(test)]
#[path = "knowledge_client_tests.rs"]
mod knowledge_client_tests;
