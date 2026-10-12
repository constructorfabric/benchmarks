//! Outbound transport shared by the chat adapters (DESIGN section 3.2
//! `llm_provider`, section 3.5 "LLM Provider", ADR-0005).
//!
//! [`ProviderTransport::send`] serializes a request body, sends it through the
//! in-process OAGW client to `/{alias}{api_path}` (the `{model}` placeholder
//! replaced by the provider model id) under the gear's S2S security context
//! (the upstreams are registered under it), and maps a non-2xx response or a
//! gateway error to an [`LlmFailure`] ([`map_http_error`],
//! [`map_gateway_error`]: 429 is `rate_limited` with the numeric
//! `Retry-After`, a gateway timeout is `provider_timeout`, anything else is a
//! sanitized `provider_error`).
//!
//! Failures before the outbound call (body serialization, HTTP request build,
//! S2S context) are [`LlmFailure::not_sent`]: the request never left the gear,
//! so the turn is settled as a pre-provider failure.

use std::sync::Arc;

use futures::StreamExt;
use http::{HeaderName, HeaderValue, Method, Request, Response, header};
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::body::Body;
use serde_json::Value;
use tracing::{debug, warn};

use super::openai_responses::{map_gateway_error, map_http_error, request_uri};
use crate::domain::llm::{LlmFailure, ProviderTarget};
use crate::infra::oagw_provisioning::SystemContextSource;

/// Error bodies are read up to this size.
pub const ERROR_BODY_LIMIT: usize = 64 * 1024;
/// Non-streaming response bodies are read up to this size.
pub const COMPLETION_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// Message of a failure before the outbound call (body or request build).
pub const INVALID_REQUEST_MESSAGE: &str = "Provider request could not be built";
/// Message of a missing S2S context.
pub const UNAUTHORIZED_MESSAGE: &str = "Provider request could not be authorized";
/// Message of a request cancelled before the provider answered.
pub const CANCELLED_MESSAGE: &str = "Provider request cancelled";
/// Message of a non-streaming body that cannot be parsed.
pub const INVALID_RESPONSE_MESSAGE: &str = "Provider returned an invalid response";

/// OAGW client and S2S context source of the chat adapters.
#[derive(Clone)]
pub struct ProviderTransport {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    system: Arc<dyn SystemContextSource>,
}

impl std::fmt::Debug for ProviderTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderTransport").finish_non_exhaustive()
    }
}

impl ProviderTransport {
    /// Transport over the OAGW client and the S2S context source.
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        system: Arc<dyn SystemContextSource>,
    ) -> Self {
        Self { gateway, system }
    }

    /// Sends `body` as a `POST` to the target's chat endpoint for `model`
    /// with the extra `headers`; a non-2xx response is mapped to a failure
    /// (module docs).
    ///
    /// # Errors
    ///
    /// [`LlmFailure`] for a failure before the outbound call (not sent), a
    /// gateway error or a non-2xx response.
    pub async fn send(
        &self,
        target: &ProviderTarget,
        model: &str,
        body: &Value,
        stream: bool,
        headers: &[(&'static str, String)],
    ) -> Result<Response<Body>, LlmFailure> {
        let body =
            serde_json::to_vec(body).map_err(|_| LlmFailure::not_sent(INVALID_REQUEST_MESSAGE))?;
        let uri = request_uri(&target.alias, &target.api_path, model);
        let http_request = build_http_request(&uri, body, stream, headers)?;
        let ctx = self.system.system_context().await.map_err(|err| {
            warn!(target: "mini_chat.llm", provider = %target.provider_id, error = %err, "S2S context unavailable for a provider request");
            LlmFailure::not_sent(UNAUTHORIZED_MESSAGE)
        })?;
        let response = self
            .gateway
            .proxy_request(ctx, http_request)
            .await
            .map_err(|err| map_gateway_error(&target.provider_id, err))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let failure = error_response_failure(response).await;
        debug!(
            target: "mini_chat.llm",
            provider = %target.provider_id,
            code = failure.kind.code(),
            provider_code = failure.provider_code.as_deref().unwrap_or_default(),
            "provider request failed"
        );
        Err(failure)
    }
}

/// The `POST` request of `uri` with a JSON body, the `Accept` header of the
/// mode and the extra `headers`.
///
/// # Errors
///
/// [`LlmFailure::not_sent`] when the request cannot be built.
pub fn build_http_request(
    uri: &str,
    body: Vec<u8>,
    stream: bool,
    headers: &[(&'static str, String)],
) -> Result<Request<Body>, LlmFailure> {
    let accept = if stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, accept);
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| LlmFailure::not_sent(INVALID_REQUEST_MESSAGE))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| LlmFailure::not_sent(INVALID_REQUEST_MESSAGE))?;
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(body))
        .map_err(|_| LlmFailure::not_sent(INVALID_REQUEST_MESSAGE))
}

/// Failure of a non-2xx response (body read up to [`ERROR_BODY_LIMIT`]).
pub async fn error_response_failure(response: Response<Body>) -> LlmFailure {
    let source = response.extensions().get::<ErrorSource>().copied();
    let (parts, body) = response.into_parts();
    let body = read_limited(body, ERROR_BODY_LIMIT).await;
    map_http_error(parts.status, &parts.headers, source, &body)
}

/// The body up to `limit` bytes; a read error ends it early.
pub async fn read_limited(body: Body, limit: usize) -> Vec<u8> {
    let mut stream = body.into_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
        if buf.len() >= limit {
            buf.truncate(limit);
            break;
        }
    }
    buf
}
