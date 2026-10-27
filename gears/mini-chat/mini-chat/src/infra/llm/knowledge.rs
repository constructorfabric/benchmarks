//! Knowledge search runtime: organization-level knowledge base search
//! through a provider vector store (`knowledge_search.enabled`).

use std::sync::Arc;

use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::Value;
use toolkit_security::SecurityContext;

use super::LlmProviderError;
use super::providers::map_gateway_error;
use super::resolver::ProviderRegistry;

/// The knowledge search runtime (config + resolution per request).
pub struct KnowledgeSearchRuntime {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    registry: Arc<ProviderRegistry>,
    provider_id: String,
    vector_store_id: String,
    top_k: usize,
    max_chunk_chars: usize,
}

impl KnowledgeSearchRuntime {
    /// Create the runtime.
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        registry: Arc<ProviderRegistry>,
        provider_id: String,
        vector_store_id: String,
        top_k: usize,
        max_chunk_chars: usize,
    ) -> Self {
        Self {
            gateway,
            registry,
            provider_id,
            vector_store_id,
            top_k,
            max_chunk_chars,
        }
    }

    /// Whether the knowledge-search parameters can be built for a tenant:
    /// the provider entry exists, has a supported kind, an upstream alias
    /// for the tenant and a non-empty `api_version`.
    #[must_use]
    pub fn available_for(&self, tenant_id: &str) -> bool {
        let Some(entry) = self.registry.entry(&self.provider_id) else {
            return false;
        };
        let kind_ok = matches!(
            entry.kind,
            Some(
                crate::config::ProviderKind::OpenaiResponses
                    | crate::config::ProviderKind::AnthropicMessages
            )
        );
        if !kind_ok {
            return false;
        }
        if entry.api_version.as_deref().is_none_or(str::is_empty) {
            return false;
        }
        self.registry.resolve(&self.provider_id, tenant_id).is_some()
    }

    /// Search the knowledge base.
    pub async fn search(
        &self,
        ctx: &SecurityContext,
        tenant_id: &str,
        query: &str,
        top_k: Option<usize>,
    ) -> Result<Vec<String>, LlmProviderError> {
        let Some(provider) = self.registry.resolve(&self.provider_id, tenant_id) else {
            return Err(LlmProviderError::Unavailable(
                "knowledge provider unresolved".to_owned(),
            ));
        };
        let entry = self
            .registry
            .entry(&self.provider_id)
            .ok_or_else(|| LlmProviderError::Unavailable("knowledge provider missing".to_owned()))?;
        let api_version = entry.api_version.clone().unwrap_or_default();
        let top_k = top_k.unwrap_or(self.top_k).min(self.top_k);
        let body = serde_json::to_vec(&serde_json::json!({
            "query": query,
            "max_num_results": top_k,
        }))
        .unwrap_or_default();
        let req = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!(
                "/{}/openai/vector_stores/{}/search?api-version={}",
                provider.alias, self.vector_store_id, api_version
            ))
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .map_err(|e| LlmProviderError::InvalidResponse(format!("request build failed: {e}")))?;
        let resp = self
            .gateway
            .proxy_request(ctx.clone(), req)
            .await
            .map_err(map_gateway_error)?;
        let status = resp.status();
        let bytes = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| LlmProviderError::Unavailable(format!("failed to read response: {e}")))?;
        if !status.is_success() {
            let message = super::providers::parse_provider_error_message(&bytes)
                .unwrap_or_else(|| format!("knowledge search failed with HTTP {}", status.as_u16()));
            return Err(LlmProviderError::Provider {
                status: Some(status.as_u16()),
                message: crate::infra::sanitize::sanitize_provider_message(&message),
                retry_after: None,
                usage: None,
                provider_response_id: None,
            });
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmProviderError::InvalidResponse(format!("invalid JSON: {e}")))?;
        let mut chunks = Vec::new();
        if let Some(data) = value.get("data").and_then(Value::as_array) {
            for item in data {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if part.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(text) = part.get("text").and_then(Value::as_str)
                        {
                            chunks.push(text.chars().take(self.max_chunk_chars).collect());
                        }
                    }
                }
            }
        }
        Ok(chunks)
    }
}
