//! The OAGW-backed provider: sends the adapter's request through the in-process gateway client
//! and translates the response.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use oagw_sdk::ServiceGatewayError;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{
    Body, ServerEvent, ServerEventsResponse, ServerEventsStream, ServiceGatewayClientV1,
};
use toolkit_security::SecurityContext;
use tracing::warn;

use super::adapter::{Adapter, EventParser};
use super::common::error_message;
use crate::domain::llm::{
    LlmCompletion, LlmEvent, LlmProvider, LlmRequest, LlmStream, ProviderError,
};

/// Largest provider error body read into memory.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// A provider entry resolved for a tenant.
pub struct OagwProvider {
    provider_id: String,
    alias: String,
    api_path: String,
    adapter: Arc<dyn Adapter>,
    gateway: Arc<dyn ServiceGatewayClientV1>,
}

impl OagwProvider {
    /// Creates the provider; requests go to `/{alias}{api_path}`.
    #[must_use]
    pub fn new(
        provider_id: String,
        alias: String,
        api_path: String,
        adapter: Arc<dyn Adapter>,
        gateway: Arc<dyn ServiceGatewayClientV1>,
    ) -> Self {
        Self {
            provider_id,
            alias,
            api_path,
            adapter,
            gateway,
        }
    }

    fn uri(&self, model: &str) -> String {
        format!("/{}{}", self.alias, self.api_path.replace("{model}", model))
    }

    async fn send(
        &self,
        ctx: &SecurityContext,
        request: &LlmRequest,
        stream: bool,
    ) -> Result<http::Response<Body>, ProviderError> {
        let body = self.adapter.build_body(request, stream)?;
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| ProviderError::provider("the provider request could not be built"))?;
        let mut builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(self.uri(&request.model))
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::ACCEPT,
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            );
        for (name, value) in self.adapter.extra_headers() {
            builder = builder.header(name, value);
        }
        let http_request = builder
            .body(Body::from(bytes))
            .map_err(|_| ProviderError::provider("the provider request could not be built"))?;
        let response = self
            .gateway
            .proxy_request(ctx.clone(), http_request)
            .await
            .map_err(|err| map_gateway_error(&self.provider_id, err))?;
        if response.status().is_success() {
            return Ok(response);
        }
        Err(http_error(&self.provider_id, response).await)
    }
}

fn map_gateway_error(
    provider_id: &str,
    err: toolkit_canonical_errors::CanonicalError,
) -> ProviderError {
    match ServiceGatewayError::from(err) {
        ServiceGatewayError::Timeout => {
            ProviderError::timeout("The provider did not answer in time")
        }
        ServiceGatewayError::RateLimited { retry_after_secs } => {
            ProviderError::rate_limited(retry_after_secs)
        }
        other => {
            warn!(provider = provider_id, error = %other, "the gateway refused the provider request");
            ProviderError::provider("The provider is currently unavailable")
        }
    }
}

fn retry_after_secs(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// Maps a non-success provider response: 429 to `rate_limited`, the gateway's own 504 to
/// `provider_timeout`, everything else to `provider_error` with the provider's message.
async fn http_error(provider_id: &str, response: http::Response<Body>) -> ProviderError {
    let status = response.status();
    let from_gateway = response.extensions().get::<ErrorSource>() == Some(&ErrorSource::Gateway);
    let retry_after = retry_after_secs(response.headers());
    let (_, body) = response.into_parts();
    let bytes = match body.into_bytes().await {
        Ok(bytes) => bytes,
        Err(_) => bytes::Bytes::new(),
    };
    let bytes = &bytes[..bytes.len().min(MAX_ERROR_BODY_BYTES)];
    if status == http::StatusCode::TOO_MANY_REQUESTS {
        return ProviderError::rate_limited(retry_after);
    }
    if status == http::StatusCode::GATEWAY_TIMEOUT && from_gateway {
        return ProviderError::timeout("The provider did not answer in time");
    }
    let message = error_message(bytes);
    warn!(
        provider = provider_id,
        status = status.as_u16(),
        "the provider answered with an error status"
    );
    ProviderError::provider(if message.is_empty() {
        format!("The provider answered with HTTP {}", status.as_u16())
    } else {
        message
    })
}

struct StreamState {
    events: ServerEventsStream<ServerEvent>,
    parser: Box<dyn EventParser>,
    queue: VecDeque<LlmEvent>,
    done: bool,
}

#[async_trait]
impl LlmProvider for OagwProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    async fn stream(
        &self,
        ctx: &SecurityContext,
        request: &LlmRequest,
    ) -> Result<LlmStream, ProviderError> {
        let response = self.send(ctx, request, true).await?;
        let events = match ServerEventsStream::from_response::<ServerEvent>(response) {
            ServerEventsResponse::Events(events) => events,
            ServerEventsResponse::Response(_) => {
                return Err(ProviderError::provider(
                    "The provider did not answer with an event stream",
                ));
            }
        };
        let state = StreamState {
            events,
            parser: self.adapter.parser(),
            queue: VecDeque::new(),
            done: false,
        };
        Ok(Box::pin(futures::stream::unfold(
            state,
            |mut state| async move {
                loop {
                    if let Some(event) = state.queue.pop_front() {
                        if matches!(event, LlmEvent::Completed { .. } | LlmEvent::Failed { .. }) {
                            state.done = true;
                            state.queue.clear();
                        }
                        return Some((Ok(event), state));
                    }
                    if state.done {
                        return None;
                    }
                    match state.events.next().await {
                        Some(Ok(event)) => {
                            let produced = state.parser.push(event.event.as_deref(), &event.data);
                            state.queue.extend(produced);
                        }
                        Some(Err(err)) => {
                            state.done = true;
                            warn!(error = %err, "the provider event stream broke");
                            return Some((
                                Err(ProviderError::provider(
                                    "The provider stream was interrupted",
                                )),
                                state,
                            ));
                        }
                        None => {
                            state.done = true;
                            let produced = state.parser.finish();
                            state.queue.extend(produced);
                        }
                    }
                }
            },
        )))
    }

    async fn complete(
        &self,
        ctx: &SecurityContext,
        request: &LlmRequest,
    ) -> Result<LlmCompletion, ProviderError> {
        let response = self.send(ctx, request, false).await?;
        let bytes = response
            .into_body()
            .into_bytes()
            .await
            .map_err(|_| ProviderError::provider("The provider response could not be read"))?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| ProviderError::provider("The provider answered with invalid JSON"))?;
        self.adapter.parse_completion(&json)
    }
}
