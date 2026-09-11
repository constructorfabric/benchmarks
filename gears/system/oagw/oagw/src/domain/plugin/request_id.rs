//! Request-id transform plugin: generates and propagates a correlation id.

use super::{PluginRequestContext, PluginResponseContext, TransformPlugin};
use crate::domain::error::OagwError;
use crate::gts_helpers;
use async_trait::async_trait;

/// Header carrying the correlation identifier.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Built-in transform plugin generating and propagating the correlation id.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        gts_helpers::TRANSFORM_REQUEST_ID
    }

    async fn transform_request(
        &self,
        context: &mut PluginRequestContext,
        _config: &serde_json::Value,
    ) -> Result<(), OagwError> {
        if context.request_id.trim().is_empty() {
            context.request_id = crate::domain::identifiers::new_uuid().to_string();
        }
        if let Ok(value) = http::HeaderValue::try_from(context.request_id.as_str()) {
            context.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }

    async fn transform_response(
        &self,
        context: &mut PluginResponseContext,
        _config: &serde_json::Value,
    ) -> Result<(), OagwError> {
        if let Ok(value) = http::HeaderValue::try_from(context.request_id.as_str()) {
            context.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "request_id_tests.rs"]
mod tests;
