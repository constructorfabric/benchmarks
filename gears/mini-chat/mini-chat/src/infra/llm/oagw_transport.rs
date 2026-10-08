//! `ProviderTransport` over the in-process OAGW proxy client.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use toolkit_security::SecurityContext;

use super::{ProviderRequest, ProviderResponse, ProviderTransport, TransportError};

pub struct OagwTransport {
    gw: Arc<dyn ServiceGatewayClientV1>,
}

impl OagwTransport {
    #[must_use]
    pub fn new(gw: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self { gw }
    }
}

#[async_trait]
impl ProviderTransport for OagwTransport {
    async fn send(
        &self,
        ctx: SecurityContext,
        req: ProviderRequest,
    ) -> Result<ProviderResponse, TransportError> {
        let uri = format!("/{}{}", req.alias, req.path);
        let mut b = http::Request::builder().method(req.method).uri(uri);
        if let Some(ct) = &req.content_type {
            b = b.header(http::header::CONTENT_TYPE, ct);
        }
        if let Some(a) = &req.accept {
            b = b.header(http::header::ACCEPT, a);
        }
        let body = if req.body.is_empty() {
            Body::Empty
        } else {
            Body::from(req.body)
        };
        let request = b
            .body(body)
            .map_err(|e| TransportError::Other(e.to_string()))?;
        match self.gw.proxy_request(ctx, request).await {
            Ok(resp) => {
                let gateway =
                    resp.extensions().get::<ErrorSource>().copied() == Some(ErrorSource::Gateway);
                let status = resp.status().as_u16();
                let headers = resp.headers().clone();
                let stream = resp
                    .into_body()
                    .into_stream()
                    .map(|r| r.map_err(|e| e.to_string()))
                    .boxed();
                Ok(ProviderResponse {
                    status,
                    headers,
                    gateway,
                    body: stream,
                })
            }
            Err(e) => {
                let status = e.status_code();
                let msg = e.to_string();
                Err(match status {
                    504 => TransportError::Timeout(msg),
                    502 | 503 => TransportError::Unavailable(msg),
                    _ => TransportError::Other(msg),
                })
            }
        }
    }
}
