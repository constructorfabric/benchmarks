//! `RequestIdTransformPlugin` — `X-Request-ID` propagation (ADR-0002).
//!
//! Request phase: propagate the inbound `X-Request-ID` or mint a new one.
//! Response phase: echo the same value back to the caller.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::OagwResult;
use crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;
use crate::domain::plugin::{
    ErrorContext, RequestContext, ResponseContext, TransformPlugin,
};

/// Header carrying the correlation id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Propagates or mints `X-Request-ID`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestIdTransformPlugin;

/// Returns the request id currently set on `ctx`, minting and storing one when
/// absent.
#[must_use]
pub fn ensure_request_id(ctx: &mut RequestContext) -> String {
    if let Some(existing) = ctx.header(REQUEST_ID_HEADER) {
        return existing.to_owned();
    }
    let generated = format!("req_{}", Uuid::new_v4());
    if let Ok(value) = http::HeaderValue::from_str(&generated) {
        ctx.headers.insert(REQUEST_ID_HEADER, value);
    }
    generated
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> OagwResult<()> {
        let _ = ensure_request_id(ctx);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> OagwResult<()> {
        if let Some(request_id) = ctx
            .config
            .get("__request_id")
            .and_then(serde_json::Value::as_str)
        {
            if let Ok(value) = http::HeaderValue::from_str(request_id) {
                ctx.headers.insert(REQUEST_ID_HEADER, value);
            }
        }
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> OagwResult<()> {
        // Errors carry the same correlation id the request phase minted, when
        // one is still available in the caller's headers.
        if let Some(request_id) = ctx.headers.get(REQUEST_ID_HEADER).cloned() {
            ctx.headers.insert(REQUEST_ID_HEADER, request_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::request_context;
    use bytes::Bytes;

    #[tokio::test]
    async fn mints_request_id_when_absent() {
        let mut ctx = request_context();
        let _ = ensure_request_id(&mut ctx);
        let minted = ctx.header(REQUEST_ID_HEADER).unwrap().to_owned();
        assert!(minted.starts_with("req_"));

        let mut response = ResponseContext {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Bytes::new(),
            is_error: false,
            config: serde_json::json!({"__request_id": minted}),
        };
        RequestIdTransformPlugin.transform_response(&mut response).await.unwrap();
        assert_eq!(
            response.headers.get(REQUEST_ID_HEADER).unwrap(),
            minted.as_str()
        );
    }

    #[tokio::test]
    async fn propagates_inbound_request_id() {
        let mut ctx = request_context();
        ctx.headers.insert("x-request-id", http::HeaderValue::from_static("client-set-id"));
        assert_eq!(ensure_request_id(&mut ctx), "client-set-id");
    }
}
