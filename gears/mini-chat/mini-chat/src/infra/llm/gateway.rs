//! OAGW access with the gear's S2S security context (research R1).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

/// Classified proxy failure.
#[derive(Debug, Clone)]
pub enum ProxyFailure {
    /// No S2S context yet (gear not started).
    NotReady,
    /// The gateway timed out waiting for the upstream.
    Timeout(String),
    /// Any other gateway failure.
    Gateway(String),
}

/// Buffered proxy response.
#[derive(Debug, Clone)]
pub struct BufferedResponse {
    /// HTTP status.
    pub status: http::StatusCode,
    /// Headers.
    pub headers: http::HeaderMap,
    /// Body.
    pub body: Bytes,
    /// Whether the gateway (not the upstream) produced the response.
    pub from_gateway: bool,
}

impl BufferedResponse {
    /// JSON body.
    #[must_use]
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// OAGW client plus the S2S context used for every provider call.
pub struct Gateway {
    client: Arc<dyn ServiceGatewayClientV1>,
    s2s: RwLock<Option<SecurityContext>>,
}

impl Gateway {
    /// New gateway.
    #[must_use]
    pub fn new(client: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self { client, s2s: RwLock::new(None) }
    }

    /// Raw OAGW client.
    #[must_use]
    pub fn client(&self) -> &Arc<dyn ServiceGatewayClientV1> {
        &self.client
    }

    /// Sets the S2S context.
    pub fn set_context(&self, ctx: SecurityContext) {
        if let Ok(mut g) = self.s2s.write() {
            *g = Some(ctx);
        }
    }

    /// Current S2S context.
    #[must_use]
    pub fn context(&self) -> Option<SecurityContext> {
        self.s2s.read().ok().and_then(|g| g.clone())
    }

    /// Sends a request and returns the streaming response.
    ///
    /// # Errors
    /// [`ProxyFailure`].
    pub async fn send(
        &self,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, ProxyFailure> {
        let ctx = self.context().ok_or(ProxyFailure::NotReady)?;
        match self.client.proxy_request(ctx, req).await {
            Ok(r) => Ok(r),
            Err(e @ CanonicalError::DeadlineExceeded { .. }) => Err(ProxyFailure::Timeout(e.to_string())),
            Err(e) => Err(ProxyFailure::Gateway(e.to_string())),
        }
    }

    /// Sends a request and buffers the response body (bounded by `timeout`).
    ///
    /// # Errors
    /// [`ProxyFailure`].
    pub async fn send_buffered(
        &self,
        req: http::Request<Body>,
        timeout: Duration,
    ) -> Result<BufferedResponse, ProxyFailure> {
        let fut = async {
            let resp = self.send(req).await?;
            let from_gateway =
                resp.extensions().get::<ErrorSource>().copied() == Some(ErrorSource::Gateway);
            let status = resp.status();
            let headers = resp.headers().clone();
            let body = resp
                .into_body()
                .into_bytes()
                .await
                .map_err(|e| ProxyFailure::Gateway(format!("body read failed: {e}")))?;
            Ok(BufferedResponse { status, headers, body, from_gateway })
        };
        tokio::time::timeout(timeout, fut)
            .await
            .unwrap_or_else(|_| Err(ProxyFailure::Timeout("request timed out".to_owned())))
    }
}

/// Builds a JSON request.
///
/// # Panics
/// Never for valid URIs built by the gear.
#[must_use]
pub fn json_request(method: http::Method, uri: &str, body: &serde_json::Value) -> http::Request<Body> {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap_or_default()))
        .unwrap_or_else(|_| http::Request::new(Body::Empty))
}

/// Builds a body-less request.
#[must_use]
pub fn empty_request(method: http::Method, uri: &str) -> http::Request<Body> {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::Empty)
        .unwrap_or_else(|_| http::Request::new(Body::Empty))
}
