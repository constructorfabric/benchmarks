//! `request_id` transform plugin: `X-Request-ID` propagation.

use crate::domain::plugin::{ErrorContext, PluginError, RequestContext, ResponseContext};

/// Transform plugin that propagates or mints a request id.
pub struct RequestIdTransformPlugin;

#[async_trait::async_trait]
impl crate::domain::plugin::TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        crate::ids::TRANSFORM_REQUEST_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let request_id = ctx
            .header(REQUEST_ID_TRANSFORM_HEADER)
            .map_or_else(generate_id, str::to_owned);
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(REQUEST_ID_TRANSFORM_HEADER),
            axum::http::HeaderValue::from_str(&request_id),
        ) {
            ctx.headers.insert(name, value);
        }
        ctx.attributes.insert("request_id".to_owned(), request_id);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        if let Some(request_id) = ctx.attributes.get("request_id")
            && let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::try_from(REQUEST_ID_TRANSFORM_HEADER),
                axum::http::HeaderValue::from_str(request_id),
            )
        {
            ctx.headers.insert(name, value);
        }
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        ctx.attributes
            .entry("request_id".to_owned())
            .or_insert_with(generate_id);
        Ok(())
    }
}

/// Canonical lowercase name of the request-id header.
pub const REQUEST_ID_TRANSFORM_HEADER: &str = "x-request-id";

/// Mint a request id when the client did not send one.
fn generate_id() -> String {
    format!("req_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
#[path = "request_id_transform_tests.rs"]
mod request_id_transform_tests;
