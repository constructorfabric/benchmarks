//! Built-in `request_id` transform plugin: propagates `X-Request-ID` and adds one on the response
//! when the upstream did not provide it.

use serde_json::Value;

use crate::domain::gts_helpers;
use crate::domain::plugin::{ProxyRequest, ProxyResponse, TransformPlugin};

/// Header the plugin propagates.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The [`TransformPlugin`] propagating request ids.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait::async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        gts_helpers::TRANSFORM_REQUEST_ID
    }

    fn plugin_type(&self) -> &'static str {
        "request_id"
    }

    async fn transform_request(
        &self,
        request: &mut ProxyRequest,
        _config: &Value,
    ) -> Result<(), crate::domain::plugin::PluginError> {
        if request.headers.get(REQUEST_ID_HEADER).is_none() {
            request.set_header(REQUEST_ID_HEADER, &generate_request_id());
        }
        Ok(())
    }

    async fn transform_response(
        &self,
        response: &mut ProxyResponse,
        _config: &Value,
    ) -> Result<(), crate::domain::plugin::PluginError> {
        if response.headers.get(REQUEST_ID_HEADER).is_none()
            && let Ok(value) = axum::http::HeaderValue::from_str(&generate_request_id()) {
                response.headers.insert(
                    axum::http::HeaderName::from_static(REQUEST_ID_HEADER),
                    value,
                );
            }
        Ok(())
    }

    async fn transform_error(
        &self,
        _detail: &mut String,
        _config: &Value,
    ) -> Result<(), crate::domain::plugin::PluginError> {
        Ok(())
    }
}

fn generate_request_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
#[path = "request_id_transform_tests.rs"]
mod tests;
