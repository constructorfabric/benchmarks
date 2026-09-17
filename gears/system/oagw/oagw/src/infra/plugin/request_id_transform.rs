//! The request-id transform plugin (ADR-0002 "Built-in Plugins":
//! `RequestIdTransformPlugin`).
//!
//! It stamps `X-Request-ID` with the id the *gateway* minted for the request
//! ([`ProxyRequest::request_id`](crate::domain::services::data_plane::ProxyRequest::request_id)),
//! on the request it forwards and on the response it returns. No configuration.
//!
//! # Documented deviation
//!
//! An `X-Request-ID` the caller or the upstream sent is **replaced**, not
//! propagated. The host provides no inbound correlation header, so the proxy
//! mints the id every audit record, gateway error and log line of the request
//! is joined by; propagating a caller-supplied id instead would break that join
//! and let a caller choose the correlation id an operator searches for.

use http::HeaderMap;
use http::header::HeaderName;

use super::registry::REQUEST_ID_TRANSFORM_PLUGIN_REF;
use super::traits::{PluginContext, TransformPlugin};
use crate::error::OagwError;

/// The correlation header the plugin stamps.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Stamps the proxy-minted correlation id on the request and the response.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait::async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn plugin_ref(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_REF
    }

    async fn transform_request(
        &self,
        context: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        insert(headers, context.request.request_id.as_str());
        Ok(())
    }

    async fn transform_response(
        &self,
        context: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        insert(headers, context.request.request_id.as_str());
        Ok(())
    }
}

/// Stamp the correlation id, replacing whatever was there.
fn insert(headers: &mut HeaderMap, request_id: &str) {
    let Ok(value) = http::HeaderValue::from_str(request_id) else {
        // The transport mints a UUID, which is always a valid header value;
        // an unrepresentable one is dropped rather than invented.
        return;
    };
    headers.insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_context;

    #[tokio::test]
    async fn the_request_carries_the_proxy_minted_id() {
        let mut outbound = HeaderMap::new();
        outbound.insert(REQUEST_ID_HEADER, "caller-id".parse().expect("valid"));

        RequestIdTransformPlugin
            .transform_request(
                &PluginContext {
                    config: None,
                    request: &test_context(),
                },
                &mut outbound,
            )
            .await
            .expect("stamping cannot fail");

        assert_eq!(
            outbound
                .get(REQUEST_ID_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("01JREQUESTID"),
            "the caller's id is replaced, not propagated"
        );
    }

    #[tokio::test]
    async fn the_response_carries_the_same_id() {
        let mut response = HeaderMap::new();
        response.insert(REQUEST_ID_HEADER, "upstream-id".parse().expect("valid"));

        RequestIdTransformPlugin
            .transform_response(
                &PluginContext {
                    config: None,
                    request: &test_context(),
                },
                &mut response,
            )
            .await
            .expect("stamping cannot fail");

        assert_eq!(
            response
                .get(REQUEST_ID_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("01JREQUESTID")
        );
    }

    #[test]
    fn the_plugin_is_registered_under_the_request_id_identifier() {
        assert_eq!(
            RequestIdTransformPlugin.plugin_ref(),
            REQUEST_ID_TRANSFORM_PLUGIN_REF
        );
    }
}
