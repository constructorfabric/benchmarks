//! `request_id` transform plugin — `X-Request-ID` propagation.
//!
//! The plugin carries an outbound `X-Request-ID` onto the request (generating
//! one when the inbound request has none) and copies the upstream's
//! `X-Request-ID` (or `X-Amzn-Trace-Id`) back onto the client response.

use crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;
use crate::domain::plugin::{
    ErrorContext, PluginResult, RequestContext, ResponseContext, TransformPlugin, async_trait,
};

const REQUEST_ID_HEADER: &str = "x-request-id";

/// The `request_id` transform plugin.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdTransformPlugin;

impl RequestIdTransformPlugin {
    fn generated() -> String {
        format!("oagw-{}", uuid::Uuid::new_v4())
    }
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    #[allow(clippy::unnecessary_literal_bound)] // trait declares `&str`; returns a literal
    fn plugin_type(&self) -> &str {
        "request_id"
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        if !ctx.headers.contains_key(REQUEST_ID_HEADER) {
            ctx.headers.insert(
                REQUEST_ID_HEADER,
                http::HeaderValue::from_str(&Self::generated()).map_err(|e| {
                    crate::domain::plugin::PluginError::Internal {
                        detail: format!("invalid request id header: {e}"),
                    }
                })?,
            );
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> PluginResult<()> {
        if !ctx.headers.contains_key(REQUEST_ID_HEADER) {
            let id = ctx
                .headers
                .get("x-amzn-trace-id")
                .cloned()
                .unwrap_or_else(|| http::HeaderValue::from_static(""));
            if !id.is_empty() {
                ctx.headers.insert(REQUEST_ID_HEADER, id);
            }
        }
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> PluginResult<()> {
        Ok(())
    }
}
