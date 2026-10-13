//! Provider adapters behind the [`LlmClient`] port, all sent through OAGW
//! (DESIGN section 3.2 `llm_provider`, ADR-0005). The provider kind selects
//! the adapter: [`openai_responses`], [`chat_completions`],
//! [`vllm_responses`] or [`anthropic_messages`]; each builds its request,
//! translates its SSE stream (shared loop in `stream`) and parses its
//! non-streaming reply.
//!
//! Shared transport: the request goes to `/{alias}{api_path}` (`{model}`
//! replaced, query kept) with the gear's S2S identity. Status mapping (DESIGN
//! §3.3 "Streaming error codes"): 429 is `rate_limited`; a gateway timeout
//! (`DeadlineExceeded`, or the gateway's own 504) is `provider_timeout`; any
//! other failure is `provider_error`, with the provider's `error.message`
//! sanitized when the body has one.

pub mod anthropic_messages;
pub mod chat_completions;
pub mod openai_responses;
mod stream;
pub mod vllm_responses;
mod wire;

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;

use super::ServiceIdentity;
use super::types::{
    CompletionResult, LlmClient, LlmEvent, LlmRequest, PROVIDER_ERROR, ProviderError, RATE_LIMITED,
    ResolvedProvider,
};
use crate::config::ProviderKind;
use crate::domain::sanitize::sanitize_provider_message;
use stream::Translate;

/// The provider error code for an exceeded context window.
const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";

/// One adapter's HTTP request: JSON body and extra headers.
pub(super) struct WireRequest {
    pub body: Value,
    pub headers: Vec<(&'static str, String)>,
}

impl WireRequest {
    fn json(body: Value) -> Self {
        Self {
            body,
            headers: Vec::new(),
        }
    }
}

/// The adapter of a provider kind.
#[derive(Clone, Copy)]
enum Adapter {
    OpenaiResponses,
    ChatCompletions,
    VllmResponses,
    AnthropicMessages,
}

impl Adapter {
    fn of(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenaiResponses => Self::OpenaiResponses,
            ProviderKind::OpenaiChatCompletions => Self::ChatCompletions,
            ProviderKind::VllmResponses => Self::VllmResponses,
            ProviderKind::AnthropicMessages => Self::AnthropicMessages,
        }
    }

    fn request(self, req: &LlmRequest) -> WireRequest {
        match self {
            Self::OpenaiResponses => WireRequest::json(openai_responses::build_body(req)),
            Self::ChatCompletions => WireRequest::json(chat_completions::build_body(req)),
            Self::VllmResponses => WireRequest::json(vllm_responses::build_body(req)),
            Self::AnthropicMessages => anthropic_messages::build_request(req),
        }
    }

    fn translator(self) -> Box<dyn Translate> {
        match self {
            Self::OpenaiResponses => Box::<openai_responses::Translator>::default(),
            Self::ChatCompletions => Box::<chat_completions::Translator>::default(),
            Self::VllmResponses => Box::<vllm_responses::Translator>::default(),
            Self::AnthropicMessages => Box::<anthropic_messages::Translator>::default(),
        }
    }

    fn parse_completion(self, bytes: &[u8]) -> Result<CompletionResult, ProviderError> {
        match self {
            Self::OpenaiResponses => openai_responses::parse_completion(bytes),
            Self::ChatCompletions => chat_completions::parse_completion(bytes),
            Self::VllmResponses => vllm_responses::parse_completion(bytes),
            Self::AnthropicMessages => anthropic_messages::parse_completion(bytes),
        }
    }
}

/// [`LlmClient`] over the in-process OAGW client, dispatching by provider kind.
pub struct OagwLlmClient {
    gw: Arc<dyn ServiceGatewayClientV1>,
    identity: Arc<ServiceIdentity>,
}

impl OagwLlmClient {
    #[must_use]
    pub fn new(gw: Arc<dyn ServiceGatewayClientV1>, identity: Arc<ServiceIdentity>) -> Self {
        Self { gw, identity }
    }

    /// POST the wire request to the provider's chat path. `Ok(None)` when
    /// `cancel` fired before the response headers arrived (the request is
    /// dropped).
    async fn send(
        &self,
        provider: &ResolvedProvider,
        model: &str,
        wire: &WireRequest,
        cancel: Option<&CancellationToken>,
    ) -> Result<Option<http::Response<Body>>, ProviderError> {
        let ctx = self
            .identity
            .get()
            .await
            .map_err(|_| ProviderError::provider("service identity not ready"))?;
        let bytes = serde_json::to_vec(&wire.body)
            .map_err(|e| ProviderError::provider(format!("request encoding failed: {e}")))?;
        let mut builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(chat_uri(provider, model))
            .header(http::header::CONTENT_TYPE, "application/json");
        for (name, value) in &wire.headers {
            builder = builder.header(*name, value);
        }
        let req = builder
            .body(Body::from(bytes))
            .map_err(|e| ProviderError::provider(format!("invalid provider request: {e}")))?;
        let call = self.gw.proxy_request(ctx, req);
        let result = match cancel {
            Some(cancel) => tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(None),
                r = call => r,
            },
            None => call.await,
        };
        match result {
            Ok(resp) => check_status(resp).await.map(Some),
            Err(e) => Err(gateway_error(&e)),
        }
    }
}

#[async_trait]
impl LlmClient for OagwLlmClient {
    async fn stream(
        &self,
        provider: &ResolvedProvider,
        mut req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, ProviderError> {
        let adapter = Adapter::of(provider.kind);
        req.stream = true;
        let wire = adapter.request(&req);
        match self
            .send(provider, &req.model, &wire, Some(&cancel))
            .await?
        {
            Some(resp) => Ok(stream::event_stream(
                resp.into_body().into_stream(),
                adapter.translator(),
                cancel,
            )),
            None => Ok(Box::pin(futures::stream::empty())),
        }
    }

    async fn complete(
        &self,
        provider: &ResolvedProvider,
        mut req: LlmRequest,
    ) -> Result<CompletionResult, ProviderError> {
        let adapter = Adapter::of(provider.kind);
        req.stream = false;
        let wire = adapter.request(&req);
        let Some(resp) = self.send(provider, &req.model, &wire, None).await? else {
            return Err(ProviderError::provider("provider request cancelled"));
        };
        let bytes = resp.into_body().into_bytes().await.map_err(|e| {
            tracing::warn!(error = %e, "provider response body read failed");
            ProviderError::provider("provider response could not be read")
        })?;
        adapter.parse_completion(&bytes)
    }
}

/// `/{alias}{api_path}` with `{model}` replaced; the query string is kept.
fn chat_uri(provider: &ResolvedProvider, model: &str) -> String {
    let path = provider.api_path.replace("{model}", model);
    let sep = if path.starts_with('/') { "" } else { "/" };
    format!("/{}{sep}{path}", provider.alias)
}

/// A gateway pipeline error (`Err` from `proxy_request`).
pub(super) fn gateway_error(e: &CanonicalError) -> ProviderError {
    tracing::warn!(error = %e, "provider request failed in the gateway");
    match e {
        CanonicalError::DeadlineExceeded { .. } => {
            ProviderError::timeout("provider request timed out")
        }
        _ => ProviderError::provider("provider request failed"),
    }
}

/// Whether the gateway itself (not the upstream provider) produced `resp`.
pub(super) fn from_gateway(resp: &http::Response<Body>) -> bool {
    resp.extensions().get::<ErrorSource>() == Some(&ErrorSource::Gateway)
}

/// Pass a 2xx response through; map any other status.
pub(super) async fn check_status(
    resp: http::Response<Body>,
) -> Result<http::Response<Body>, ProviderError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    if status == http::StatusCode::TOO_MANY_REQUESTS {
        return Err(rate_limited(resp.headers()));
    }
    if from_gateway(&resp) {
        tracing::warn!(
            status = status.as_u16(),
            "provider request failed in the gateway"
        );
        return Err(if status == http::StatusCode::GATEWAY_TIMEOUT {
            ProviderError::timeout("provider request timed out")
        } else {
            ProviderError::provider(format!(
                "provider unavailable (gateway HTTP {})",
                status.as_u16()
            ))
        });
    }
    let bytes = resp.into_body().into_bytes().await.unwrap_or_default();
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    let mut err = parsed
        .as_ref()
        .and_then(|v| error_from_value(v.get("error")).or_else(|| error_from_value(Some(v))))
        .unwrap_or_else(|| {
            ProviderError::provider(format!("provider returned HTTP {}", status.as_u16()))
        });
    err.code = PROVIDER_ERROR;
    Err(err)
}

fn rate_limited(headers: &http::HeaderMap) -> ProviderError {
    let retry_after = headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let message = match retry_after {
        Some(secs) => format!("provider rate limit exceeded; retry after {secs} seconds"),
        None => "provider rate limit exceeded".to_owned(),
    };
    ProviderError {
        code: RATE_LIMITED,
        message,
        context_length_exceeded: false,
        retry_after_secs: retry_after,
    }
}

/// `{code, message}` object -> `provider_error` with the sanitized message.
/// `None` when `v` is not an object with a string `message` or `code`.
fn error_from_value(v: Option<&Value>) -> Option<ProviderError> {
    let obj = v?.as_object()?;
    let code = obj.get("code").and_then(Value::as_str);
    let message = obj.get("message").and_then(Value::as_str);
    if code.is_none() && message.is_none() {
        return None;
    }
    let message = message
        .filter(|m| !m.trim().is_empty())
        .map_or_else(|| "provider error".to_owned(), sanitize_provider_message);
    Some(ProviderError {
        context_length_exceeded: code == Some(CONTEXT_LENGTH_EXCEEDED),
        ..ProviderError::provider(message)
    })
}
