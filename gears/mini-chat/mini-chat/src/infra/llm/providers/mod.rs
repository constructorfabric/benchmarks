//! Provider adapters.

pub mod anthropic_messages;
pub mod openai_chat_completions;
pub mod openai_responses;
pub mod vllm_responses;

use std::sync::Arc;

use oagw_sdk::{Body, ServiceGatewayClientV1};
use toolkit_security::SecurityContext;

use super::resolver::ResolvedProvider;
use super::{LlmProviderError, LlmRequest, ProviderEventStream};
use crate::config::ProviderKind;

/// The adapter facade: builds and dispatches a chat request to the resolved
/// provider through OAGW.
pub struct ProviderAdapter {
    gateway: Arc<dyn ServiceGatewayClientV1>,
}

impl ProviderAdapter {
    /// Create the facade.
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self { gateway }
    }

    /// Send a streaming chat request; returns the internal event stream.
    pub async fn stream_chat(
        &self,
        ctx: &SecurityContext,
        provider: &ResolvedProvider,
        request: &LlmRequest,
    ) -> Result<ProviderEventStream, LlmProviderError> {
        match provider.kind {
            ProviderKind::OpenaiResponses => {
                openai_responses::stream_chat(&self.gateway, ctx, provider, request).await
            }
            ProviderKind::OpenaiChatCompletions => {
                openai_chat_completions::stream_chat(&self.gateway, ctx, provider, request).await
            }
            ProviderKind::VllmResponses => {
                vllm_responses::stream_chat(&self.gateway, ctx, provider, request).await
            }
            ProviderKind::AnthropicMessages => {
                anthropic_messages::stream_chat(&self.gateway, ctx, provider, request).await
            }
        }
    }

    /// Send a non-streaming completion (thread summary). Returns the output
    /// text, usage and provider response id.
    pub async fn complete(
        &self,
        ctx: &SecurityContext,
        provider: &ResolvedProvider,
        request: &LlmRequest,
    ) -> Result<(String, Option<mini_chat_sdk::UsageTokens>, Option<String>), LlmProviderError>
    {
        match provider.kind {
            ProviderKind::AnthropicMessages => {
                anthropic_messages::complete(&self.gateway, ctx, provider, request).await
            }
            _ => openai_responses::complete(&self.gateway, ctx, provider, request).await,
        }
    }
}

/// Build the proxied JSON request.
pub(crate) fn json_request(
    provider: &ResolvedProvider,
    path: &str,
    body: Vec<u8>,
) -> Result<http::Request<Body>, LlmProviderError> {
    http::Request::builder()
        .method(http::Method::POST)
        .uri(format!("/{}{path}", provider.alias))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::ACCEPT, "application/json")
        .body(Body::from(body))
        .map_err(|e| LlmProviderError::InvalidResponse(format!("request build failed: {e}")))
}

/// Map a non-SSE HTTP error response to an [`LlmProviderError`].
pub(crate) async fn map_error_response(
    resp: http::Response<Body>,
) -> LlmProviderError {
    let status = resp.status().as_u16();
    let is_gateway = matches!(
        resp.extensions().get::<oagw_sdk::api::ErrorSource>(),
        Some(oagw_sdk::api::ErrorSource::Gateway)
    );
    let retry_after = resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let body = resp.into_body().into_bytes().await.unwrap_or_default();
    if is_gateway && status == 504 {
        return LlmProviderError::Timeout;
    }
    if status == 429 {
        return LlmProviderError::RateLimited { retry_after };
    }
    let message = parse_provider_error_message(&body)
        .unwrap_or_else(|| format!("provider returned HTTP {status}"));
    LlmProviderError::Provider {
        status: Some(status),
        message: crate::infra::sanitize::sanitize_provider_message(&message),
        retry_after,
        usage: None,
        provider_response_id: None,
    }
}

/// Extract a provider error message from a JSON error body.
pub(crate) fn parse_provider_error_message(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    // OpenAI shape: {"error": {"message": ...}}; flat shape: {"message": ...}.
    value
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

/// Map an OAGW pipeline error to an [`LlmProviderError`].
pub(crate) fn map_gateway_error(err: toolkit_canonical_errors::CanonicalError) -> LlmProviderError {
    use oagw_sdk::ServiceGatewayError;
    match ServiceGatewayError::from(err) {
        ServiceGatewayError::Timeout => LlmProviderError::Timeout,
        ServiceGatewayError::RateLimited { retry_after_secs } => LlmProviderError::RateLimited {
            retry_after: retry_after_secs,
        },
        other => LlmProviderError::Unavailable(format!("gateway error: {other:?}")),
    }
}

/// Parse the usage object of a terminal response.
pub(crate) fn parse_usage(response: &serde_json::Value) -> Option<mini_chat_sdk::UsageTokens> {
    let usage = response.get("usage")?;
    let get = |key: &str| usage.get(key).and_then(serde_json::Value::as_i64).unwrap_or(0);
    Some(mini_chat_sdk::UsageTokens {
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_input_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        cache_write_input_tokens: usage
            .pointer("/cache_write_input_tokens")
            .or_else(|| usage.pointer("/cache_creation_input_tokens"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .or_else(|| usage.pointer("/completion_tokens_details/reasoning_tokens"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
    })
}

/// Normalize an incomplete/truncation reason.
pub(crate) fn normalize_incomplete_reason(reason: Option<&str>) -> Option<String> {
    let reason = reason?;
    Some(
        match reason {
            "max_tokens" | "max_output_tokens" | "length" => "max_tokens",
            "content_filter" => "content_filter",
            _ => "other",
        }
        .to_owned(),
    )
}
