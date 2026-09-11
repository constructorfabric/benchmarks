//! `cf.core.oagw.request_id.v1` — `X-Request-ID` propagation.
//!
//! Propagates an inbound correlation id to the upstream, minting one when the
//! caller supplied none, and echoes it back on the response so the whole hop
//! is greppable by a single value
//! (`cpt-cf-oagw-nfr-observability`).

use async_trait::async_trait;

use crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;
use crate::domain::plugin::{PluginResult, RequestContext, ResponseContext, TransformPlugin};

/// Header carrying the correlation id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Ensures every proxied exchange carries a correlation id.
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

    async fn transform_request(&self, ctx: &mut RequestContext) -> PluginResult {
        let header = ctx
            .config_str("header")
            .unwrap_or(REQUEST_ID_HEADER)
            .to_owned();
        if !ctx.headers.contains(&header) {
            ctx.headers.set(&header, uuid::Uuid::new_v4().to_string());
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> PluginResult {
        // Nothing to do when the upstream already echoed one back.
        let header = ctx
            .config_str("header")
            .unwrap_or(REQUEST_ID_HEADER)
            .to_owned();
        if !ctx.headers.contains(&header)
            && let Some(propagated) = ctx.config_str("__propagated").map(str::to_owned)
        {
            ctx.headers.set(&header, propagated);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::{request_context, response_context};
    use serde_json::json;

    #[tokio::test]
    async fn mints_an_id_when_the_caller_supplied_none() {
        let mut ctx = request_context(serde_json::Map::new());
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .expect("transform");
        let value = ctx.headers.get(REQUEST_ID_HEADER).expect("minted");
        assert!(uuid::Uuid::parse_str(value).is_ok());
    }

    #[tokio::test]
    async fn preserves_an_inbound_id() {
        let mut ctx = request_context(serde_json::Map::new());
        ctx.headers.set(REQUEST_ID_HEADER, "req_abc123");
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .expect("transform");
        assert_eq!(ctx.headers.get(REQUEST_ID_HEADER), Some("req_abc123"));
    }

    #[tokio::test]
    async fn honours_a_custom_header_name() {
        let config = json!({ "header": "x-correlation-id" })
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut ctx = request_context(config);
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .expect("transform");
        assert!(ctx.headers.contains("x-correlation-id"));
        assert!(!ctx.headers.contains(REQUEST_ID_HEADER));
    }

    #[tokio::test]
    async fn response_phase_backfills_from_the_propagated_value() {
        let config = json!({ "__propagated": "req_abc123" })
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut ctx = response_context(config);
        RequestIdTransformPlugin
            .transform_response(&mut ctx)
            .await
            .expect("transform");
        assert_eq!(ctx.headers.get(REQUEST_ID_HEADER), Some("req_abc123"));
    }
}
