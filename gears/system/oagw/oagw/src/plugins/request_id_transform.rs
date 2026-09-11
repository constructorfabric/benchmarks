//! `RequestIdTransformPlugin` — `X-Request-ID` injection / propagation.

use async_trait::async_trait;

use super::{ErrorContext, PluginError, RequestContext, ResponseContext, TransformPlugin};

/// Request-id transform.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestIdTransformPlugin;

/// Header carrying the request identifier.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        crate::gts::transform_plugin::REQUEST_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        if ctx.headers.contains_key(REQUEST_ID_HEADER) {
            return Ok(());
        }
        let value = ctx
            .config
            .get("request_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if let Ok(value) = http::HeaderValue::from_str(&value) {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }
}
