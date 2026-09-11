// Updated: 2026-09-01 by Constructor Tech
//! `request_id` transform plugin (PRD `cpt-cf-oagw-fr-plugin-system`).
//!
//! Propagates an existing `X-Request-ID` from the caller, or mints one when the
//! caller supplied none, so a proxied exchange can be traced end to end. The
//! generated value is also the `trace_id` carried in OAGW's problem documents.

use async_trait::async_trait;

use crate::domain::plugin::{
    ErrorContext, PluginError, RequestContext, ResponseContext, TransformPlugin,
};

/// Header the request id travels in.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The request-id propagation transform.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "request_id"
    }

    fn plugin_type(&self) -> &'static str {
        crate::gts::TRANSFORM_REQUEST_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        if !ctx.headers.contains_key(REQUEST_ID_HEADER)
            && let Ok(value) = http::HeaderValue::from_str(&ctx.request_id)
        {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        if !ctx.headers.contains_key(REQUEST_ID_HEADER)
            && let Ok(value) = http::HeaderValue::from_str(
                ctx.config
                    .get("request_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
            )
        {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::{request_context, response_context};
    use http::HeaderValue;

    #[tokio::test]
    async fn mints_a_request_id_when_the_caller_supplied_none() {
        let mut ctx = request_context();
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .unwrap();
        assert_eq!(
            ctx.headers.get(REQUEST_ID_HEADER).unwrap(),
            ctx.request_id.as_str()
        );
    }

    #[tokio::test]
    async fn preserves_an_existing_request_id() {
        let mut ctx = request_context();
        ctx.headers
            .insert("x-request-id", HeaderValue::from_static("caller-provided"));
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .unwrap();
        assert_eq!(
            ctx.headers.get(REQUEST_ID_HEADER).unwrap(),
            "caller-provided"
        );
    }

    #[tokio::test]
    async fn echoes_the_request_id_on_the_response() {
        let mut ctx = response_context(Default::default());
        ctx.config.insert(
            "request_id".to_owned(),
            serde_json::Value::String("req-1".to_owned()),
        );
        RequestIdTransformPlugin
            .transform_response(&mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.headers.get(REQUEST_ID_HEADER).unwrap(), "req-1");
    }
}
