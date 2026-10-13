//! Transport to providers: the OAGW in-process proxy in production.

use std::sync::Arc;

use async_trait::async_trait;
use oagw_sdk::Body;
use oagw_sdk::api::ErrorSource;
use std::sync::RwLock;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

/// Gateway-side failures (the provider response itself is `Ok`).
#[derive(Debug, Clone, thiserror::Error)]
pub enum TransportError {
    #[error("gateway timeout: {0}")]
    Timeout(String),
    #[error("gateway error: {0}")]
    Gateway(String),
}

/// Sends provider requests. The URI is `/{alias}{path}`.
#[async_trait]
pub trait ProviderTransport: Send + Sync {
    /// Returns the provider response (any status) or a gateway failure.
    async fn send(&self, req: http::Request<Body>) -> Result<http::Response<Body>, TransportError>;
}

/// OAGW-backed transport. Requests run under the gear's S2S security
/// context (set at start), so upstreams registered by the gear resolve for
/// users of every tenant.
pub struct OagwTransport {
    gateway: Arc<dyn oagw_sdk::ServiceGatewayClientV1>,
    ctx: RwLock<Option<SecurityContext>>,
}

impl OagwTransport {
    #[must_use]
    pub fn new(gateway: Arc<dyn oagw_sdk::ServiceGatewayClientV1>) -> Self {
        Self {
            gateway,
            ctx: RwLock::new(None),
        }
    }

    pub fn set_context(&self, ctx: SecurityContext) {
        if let Ok(mut g) = self.ctx.write() {
            *g = Some(ctx);
        }
    }

    fn context(&self) -> Result<SecurityContext, TransportError> {
        if let Some(c) = self.ctx.read().ok().and_then(|g| g.clone()) {
            return Ok(c);
        }
        SecurityContext::builder()
            .subject_id(toolkit_security::constants::DEFAULT_SUBJECT_ID)
            .subject_tenant_id(toolkit_security::constants::DEFAULT_TENANT_ID)
            .build()
            .map_err(|e| TransportError::Gateway(e.to_string()))
    }
}

fn classify(err: &CanonicalError) -> TransportError {
    let s = err.to_string();
    if s.starts_with("deadline_exceeded") {
        TransportError::Timeout(s)
    } else {
        TransportError::Gateway(s)
    }
}

#[async_trait]
impl ProviderTransport for OagwTransport {
    async fn send(&self, req: http::Request<Body>) -> Result<http::Response<Body>, TransportError> {
        let ctx = self.context()?;
        let resp = self.gateway.proxy_request(ctx, req).await.map_err(|e| classify(&e))?;
        if !resp.status().is_success() && resp.extensions().get::<ErrorSource>().copied() == Some(ErrorSource::Gateway) {
            let status = resp.status();
            let body = resp.into_body().into_bytes().await.unwrap_or_default();
            let text = String::from_utf8_lossy(&body).to_string();
            let is_timeout = status == http::StatusCode::GATEWAY_TIMEOUT && text.contains("deadline_exceeded");
            return Err(if is_timeout {
                TransportError::Timeout(text)
            } else {
                TransportError::Gateway(format!("{status}: {text}"))
            });
        }
        Ok(resp)
    }
}
