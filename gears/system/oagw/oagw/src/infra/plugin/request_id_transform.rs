//! `cf.core.oagw.request_id.v1` — `X-Request-ID` propagation.
//!
//! On the request phase the inbound correlation id is forwarded to the
//! upstream, or a fresh one is minted when the client did not supply one. On
//! the response phase the same id is echoed back so a caller can correlate
//! its own logs with the gateway's without reading the body.

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue};

use crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;
use crate::domain::plugin::{PluginError, RequestContext, ResponseContext, TransformPlugin};

/// Header carrying the correlation id.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// `X-Request-ID` propagation transform.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    fn phases(&self) -> &[&str] {
        &["on_request", "on_response"]
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        if !ctx.headers.contains_key(&REQUEST_ID_HEADER) {
            let generated = uuid::Uuid::new_v4().to_string();
            let value = HeaderValue::from_str(&generated).map_err(|_| {
                PluginError::Internal("generated request id is not a valid header".to_owned())
            })?;
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        if let Some(value) = ctx.request_headers.get(&REQUEST_ID_HEADER).cloned() {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::PluginConfig;
    use axum::http::{HeaderMap, Method, StatusCode};
    use bytes::Bytes;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn request(headers: HeaderMap) -> RequestContext {
        RequestContext {
            security_context: SecurityContext::anonymous(),
            method: Method::GET,
            path: "/v1/x".to_owned(),
            query: vec![],
            headers,
            body: Bytes::new(),
            config: PluginConfig::new(),
            upstream_alias: "api.example.com".to_owned(),
            upstream_id: Uuid::nil(),
        }
    }

    #[tokio::test]
    async fn inbound_request_id_is_preserved() {
        let mut headers = HeaderMap::new();
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("req_abc123"));
        let mut ctx = request(headers);
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.headers.get(&REQUEST_ID_HEADER).unwrap(), "req_abc123");
    }

    #[tokio::test]
    async fn a_missing_request_id_is_minted() {
        let mut ctx = request(HeaderMap::new());
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .unwrap();
        let minted = ctx.headers.get(&REQUEST_ID_HEADER).expect("minted");
        assert!(Uuid::parse_str(minted.to_str().unwrap()).is_ok());
    }

    #[tokio::test]
    async fn response_echoes_the_request_id() {
        let mut request_headers = HeaderMap::new();
        request_headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("req_abc123"));
        let mut ctx = ResponseContext {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            config: PluginConfig::new(),
            request_headers,
        };
        RequestIdTransformPlugin
            .transform_response(&mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.headers.get(&REQUEST_ID_HEADER).unwrap(), "req_abc123");
    }
}
