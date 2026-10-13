//! The provider client over the OAGW in-process proxy
//! (`ServiceGatewayClientV1::proxy_request`): builds the request with the
//! adapter of the provider kind, sends it with the request's
//! `SecurityContext`, classifies a failed call and wraps a streaming body in
//! a [`ProviderStream`] that parses it incrementally.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::body::BodyStream;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use toolkit_security::SecurityContext;

use super::classify::{
    HttpResponseInfo, Source, classify_body_error, classify_call_error, classify_response,
};
use super::providers::{EventTranslator, ProviderAdapters};
use super::sse_parse::SseParser;
use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::{
    BuiltRequest, ProviderClient, ProviderStream, ToolCallOutput,
};
use crate::domain::service::stream::request::ProviderRequestInput;
use crate::domain::service::stream::sanitize::sanitize_error;
use crate::domain::service::stream::types::{ProviderEvent, ProviderFailure, StreamErrorCode};

/// OAGW-backed provider client.
pub struct OagwProviderClient {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    adapters: ProviderAdapters,
}

impl OagwProviderClient {
    /// The client with the standard adapters.
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self::with_adapters(oagw, ProviderAdapters::standard())
    }

    /// The client with explicit adapters.
    #[must_use]
    pub fn with_adapters(
        oagw: Arc<dyn ServiceGatewayClientV1>,
        adapters: ProviderAdapters,
    ) -> Self {
        Self { oagw, adapters }
    }
}

/// How much of a non-streaming error body is read.
const ERROR_BODY_LIMIT: usize = 64 * 1024;

#[async_trait]
impl ProviderClient for OagwProviderClient {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-return-build-error
    fn build_request(&self, input: &ProviderRequestInput<'_>) -> Result<BuiltRequest, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-match-kind
        let adapter = self.adapters.get(input.target.kind).ok_or_else(|| {
            DomainError::Internal(format!(
                "no adapter for provider kind {:?}",
                input.target.kind
            ))
        })?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-match-kind
        adapter.build_request(input)
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-return-build-error

    fn continue_request(
        &self,
        previous: &BuiltRequest,
        outputs: &[ToolCallOutput],
    ) -> Result<BuiltRequest, DomainError> {
        let adapter = self.adapters.get(previous.kind).ok_or_else(|| {
            DomainError::Internal(format!("no adapter for provider kind {:?}", previous.kind))
        })?;
        adapter.continue_request(previous, outputs)
    }

    async fn open(
        &self,
        ctx: &SecurityContext,
        request: &BuiltRequest,
    ) -> Result<Box<dyn ProviderStream>, ProviderFailure> {
        let Some(adapter) = self.adapters.get(request.kind) else {
            return Err(ProviderFailure::generic(StreamErrorCode::ProviderError));
        };
        let payload = serde_json::to_vec(&request.body)
            .map_err(|_| ProviderFailure::generic(StreamErrorCode::ProviderError))?;
        let mut builder = http::Request::builder().method("POST").uri(&request.uri);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let http_request = builder
            .body(Body::from(payload))
            .map_err(|_| ProviderFailure::generic(StreamErrorCode::ProviderError))?;
        let response = self
            .oagw
            .proxy_request(ctx.clone(), http_request)
            .await
            .map_err(|error| {
                let classified = classify_call_error(&error);
                let sanitized = sanitize_error(&error.to_string())
                    .chars()
                    .take(512)
                    .collect::<String>();
                let uri_path = request.uri.split('?').next().unwrap_or("").to_owned();
                tracing::warn!(
                    code = ?classified.code,
                    detail = %sanitized,
                    uri_path = %uri_path,
                    "the provider request failed before a response"
                );
                classified
            })?;
        let (parts, body) = response.into_parts();
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let content_type = header("content-type");
        let retry_after = header("retry-after");
        let source = parts
            .extensions
            .get::<ErrorSource>()
            .map(|source| match source {
                ErrorSource::Gateway => Source::Gateway,
                ErrorSource::Upstream => Source::Upstream,
            });
        let mut info = HttpResponseInfo {
            status: parts.status.as_u16(),
            retry_after: retry_after.as_deref(),
            content_type: content_type.as_deref(),
            source,
            body: &[],
        };
        if parts.status.is_success() {
            if let Some(failure) = classify_response(&info) {
                return Err(failure);
            }
        } else {
            let bytes = read_bounded(body).await;
            info.body = &bytes;
            let raw_body = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
            let sanitized_body = sanitize_error(&raw_body)
                .chars()
                .take(512)
                .collect::<String>();
            let uri_path = request.uri.split('?').next().unwrap_or("").to_owned();
            tracing::warn!(
                status = info.status,
                body = %sanitized_body,
                uri_path = %uri_path,
                "the provider answered with an error status"
            );
            return Err(classify_response(&info)
                .unwrap_or_else(|| ProviderFailure::generic(StreamErrorCode::ProviderError)));
        }
        Ok(Box::new(OagwStream {
            body: body.into_stream(),
            parser: SseParser::new(),
            translator: adapter.translator(),
            pending: VecDeque::new(),
            done: false,
        }))
    }
}

/// Reads at most [`ERROR_BODY_LIMIT`] bytes of an error body and drops the rest.
async fn read_bounded(body: Body) -> Vec<u8> {
    let mut stream = body.into_stream();
    let mut bytes = Vec::new();
    while bytes.len() < ERROR_BODY_LIMIT {
        match stream.next().await {
            Some(Ok(chunk)) => {
                let room = ERROR_BODY_LIMIT - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Some(Err(_)) | None => break,
        }
    }
    bytes
}

/// The streamed response body of one provider request.
struct OagwStream {
    body: BodyStream,
    parser: SseParser,
    translator: Box<dyn EventTranslator>,
    pending: VecDeque<ProviderEvent>,
    done: bool,
}

#[async_trait]
impl ProviderStream for OagwStream {
    async fn next_event(&mut self) -> Option<Result<ProviderEvent, ProviderFailure>> {
        loop {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-return-translated
            if let Some(event) = self.pending.pop_front() {
                return Some(Ok(event));
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-return-translated
            if self.done {
                return None;
            }
            match self.body.next().await {
                Some(Ok(chunk)) => {
                    for event in self.parser.push(&chunk) {
                        self.pending.extend(self.translator.translate(&event));
                    }
                }
                Some(Err(error)) => {
                    self.done = true;
                    return Some(Err(classify_body_error(&*error)));
                }
                None => {
                    self.done = true;
                    for event in self.parser.finish() {
                        self.pending.extend(self.translator.translate(&event));
                    }
                    self.pending.extend(self.translator.finish());
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "oagw_client_tests.rs"]
mod tests;
