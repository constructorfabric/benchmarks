//! LLM provider library (ADR-0001/0005): transport, provider resolution, adapters, storage.

pub mod oagw_transport;
pub mod provider;
pub mod responses;
pub mod sse_parser;
pub mod storage;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use toolkit_security::SecurityContext;

/// One outbound request to a provider through OAGW (`/{alias}{path}`).
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub method: http::Method,
    pub alias: String,
    /// Path (and query) appended to the alias, starting with `/`.
    pub path: String,
    pub content_type: Option<String>,
    pub accept: Option<String>,
    pub body: Bytes,
}

impl ProviderRequest {
    #[must_use]
    pub fn json(method: http::Method, alias: &str, path: String, body: &serde_json::Value) -> Self {
        Self {
            method,
            alias: alias.to_owned(),
            path,
            content_type: Some("application/json".to_owned()),
            accept: None,
            body: Bytes::from(serde_json::to_vec(body).unwrap_or_default()),
        }
    }

    #[must_use]
    pub fn empty(method: http::Method, alias: &str, path: String) -> Self {
        Self {
            method,
            alias: alias.to_owned(),
            path,
            content_type: None,
            accept: None,
            body: Bytes::new(),
        }
    }
}

/// Response of a provider request; the body is always a stream (OAGW never buffers).
pub struct ProviderResponse {
    pub status: u16,
    pub headers: http::HeaderMap,
    /// `true` when OAGW itself produced the response (problem+json), not the provider.
    pub gateway: bool,
    pub body: BoxStream<'static, Result<Bytes, String>>,
}

impl ProviderResponse {
    /// Collects the body (non-streaming responses only).
    ///
    /// # Errors
    /// Returns the stream error text.
    pub async fn into_bytes(mut self) -> Result<Bytes, String> {
        let mut out = Vec::new();
        while let Some(chunk) = self.body.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(Bytes::from(out))
    }

    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    #[must_use]
    pub fn content_type(&self) -> String {
        self.headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        self.headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
    }
}

/// Failures raised before a response exists.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TransportError {
    #[error("gateway timeout: {0}")]
    Timeout(String),
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    #[error("transport error: {0}")]
    Other(String),
}

/// Outbound transport (production: OAGW in-process proxy; tests: scripted fake).
#[async_trait]
pub trait ProviderTransport: Send + Sync {
    async fn send(
        &self,
        ctx: SecurityContext,
        req: ProviderRequest,
    ) -> Result<ProviderResponse, TransportError>;
}
