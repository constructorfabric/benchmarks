//! The OAGW adapter of the summary transport: the non-streaming `POST` of the summary request
//! through the in-process gateway client under the gear's S2S security context
//! (`cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call`, steps `inst-ad-post`,
//! `inst-ad-transport-failed`, `inst-ad-read-error` and `inst-ad-parse`).
//!
//! The request carries `Content-Type: application/json`, `Accept: application/json` and no
//! credential header; the body is read bounded (16 KiB of an error answer, 1 MiB of a success
//! answer); a gateway error (an `Err`, or a response marked `ErrorSource::Gateway`) is
//! `TransportFailure::Gateway`, an unbound S2S context or an unreadable body is
//! `TransportFailure::Transport`; an upstream error answer (also a 429 or a 5xx of the provider)
//! is an [`HttpAnswer`].

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use oagw_sdk::Body;
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use serde_json::Value;

use crate::domain::maintenance::settings::{MAX_ERROR_BODY_BYTES, MAX_SUCCESS_BODY_BYTES};
use crate::domain::maintenance::summary_call::{HttpAnswer, SummaryTransport, TransportFailure};
use crate::infra::provider_task::S2sContext;

/// The adapter over the gateway client.
pub struct OagwSummaryClient {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
}

impl OagwSummaryClient {
    /// Create the adapter.
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContext>) -> Self {
        Self { gateway, s2s }
    }
}

#[async_trait]
impl SummaryTransport for OagwSummaryClient {
    async fn post_json(&self, uri: &str, body: &Value) -> Result<HttpAnswer, TransportFailure> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-post
        // the gear's S2S security context: OAGW injects the credentials, so the request carries
        // none; an unbound context is a transport failure
        let ctx = self.s2s.get().cloned().ok_or(TransportFailure::Transport)?;
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "application/json")
            .body(Body::from(Bytes::from(body.to_string())))
            .map_err(|_| TransportFailure::Transport)?;
        let response = self
            .gateway
            .proxy_request(ctx, request)
            .await
            .map_err(|_| TransportFailure::Gateway)?;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-post
        let status = response.status().as_u16();
        let success = (200..300).contains(&status);
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-transport-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-transport
        // a gateway error (an `Err`, or a Problem marked with the gateway as its source, a 502,
        // 503 or 504 included) is not an upstream answer
        if !success
            && matches!(
                response.extensions().get::<ErrorSource>(),
                Some(ErrorSource::Gateway)
            )
        {
            return Err(TransportFailure::Gateway);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-transport
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-transport-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-read-error
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-parse
        // at most 16 KiB of an error body (kept for the classification only) and at most 1 MiB
        // of a success body are read
        let limit = if success {
            MAX_SUCCESS_BODY_BYTES
        } else {
            MAX_ERROR_BODY_BYTES
        };
        let mut stream = response.into_body().into_stream();
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| TransportFailure::Transport)?;
            if success {
                if bytes.len() + chunk.len() > limit {
                    return Ok(HttpAnswer {
                        status,
                        body: Vec::new(),
                        too_large: true,
                    });
                }
                bytes.extend_from_slice(&chunk);
            } else {
                let room = limit - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if bytes.len() >= limit {
                    break;
                }
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-parse
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-read-error
        Ok(HttpAnswer {
            status,
            body: bytes,
            too_large: false,
        })
    }
}

#[cfg(test)]
#[path = "summary_client_tests.rs"]
mod summary_client_tests;
