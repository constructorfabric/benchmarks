// Created: 2026-08-29 by Constructor Tech
//! `cf.core.oagw.request_id.v1` — request-id propagation transform.

use async_trait::async_trait;

use crate::domain::plugin::{
    ErrorContext, PluginError, RequestContext, ResponseContext, TRANSFORM_REQUEST_ID,
    TransformPlugin,
};

/// Sets `x-request-id` on the outbound request when absent and echoes it on the
/// response.
pub struct RequestIdTransform;

impl RequestIdTransform {
    /// Header the plugin propagates.
    pub(crate) const HEADER: &'static str = "x-request-id";
}

#[async_trait]
impl TransformPlugin for RequestIdTransform {
    fn id(&self) -> &str {
        TRANSFORM_REQUEST_ID
    }

    fn plugin_type(&self) -> &str {
        "transform_plugin"
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        if !ctx
            .headers
            .contains_key(axum::http::HeaderName::from_static(Self::HEADER))
        {
            let value = ctx.security.subject_id().as_simple().to_string();
            if let Ok(header) = axum::http::HeaderValue::from_str(&value) {
                ctx.headers
                    .insert(axum::http::HeaderName::from_static(Self::HEADER), header);
            }
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        // Echo the propagated id so callers can correlate request and response.
        if let Some(inbound) = ctx.request_headers.get(Self::HEADER) {
            ctx.headers.insert(
                axum::http::HeaderName::from_static(Self::HEADER),
                inbound.clone(),
            );
        }
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        ctx.headers.insert(
            axum::http::HeaderName::from_static(Self::HEADER),
            axum::http::HeaderValue::from_static("0"),
        );
        Ok(())
    }
}
