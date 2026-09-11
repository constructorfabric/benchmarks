//! The `request_id` built-in transform plugin.
//!
//! Propagates an inbound `X-Request-ID` when present and generates one when
//! absent, then echoes the same value on the response.

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;
use crate::domain::plugin::PluginContext;

/// The header this plugin manages.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Propagates or generates `X-Request-ID`.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

impl RequestIdTransformPlugin {
    /// The value the request should carry, propagated or freshly minted.
    #[must_use]
    pub fn ensure_request_id(headers: &http::HeaderMap) -> String {
        headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned)
    }
}

#[async_trait]
impl crate::domain::plugin::TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    async fn transform_request(
        &self,
        _context: &PluginContext,
        _config: &serde_json::Value,
        inbound: &http::HeaderMap,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError> {
        // The caller's correlation id is read from what arrived: the default
        // passthrough drops every inbound header, but propagation is the
        // plugin's whole purpose.
        let value = Self::ensure_request_id(inbound);
        if let Ok(header) = http::HeaderValue::from_str(&value) {
            headers.insert(http::HeaderName::from_static(REQUEST_ID_HEADER), header);
        }
        Ok(())
    }

    async fn transform_response(
        &self,
        context: &PluginContext,
        _config: &serde_json::Value,
        _status: http::StatusCode,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError> {
        if headers.get(REQUEST_ID_HEADER).is_some() {
            return Ok(());
        }
        // The request phase settled the value; echoing it here is what lets a
        // caller correlate a reply with the request it belongs to.
        if let Some(request_id) = context.request_id.as_deref()
            && let Ok(header) = http::HeaderValue::from_str(request_id)
        {
            headers.insert(http::HeaderName::from_static(REQUEST_ID_HEADER), header);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::TransformPlugin;

    fn context() -> PluginContext {
        PluginContext {
            tenant_id: uuid::Uuid::nil(),
            subject_id: uuid::Uuid::nil(),
            upstream_id: uuid::Uuid::nil(),
            route_id: None,
            alias: "api.openai.com".into(),
            bearer_token: None,
            request_id: None,
        }
    }

    fn headers() -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        map.insert(
            http::HeaderName::from_static(REQUEST_ID_HEADER),
            http::HeaderValue::from_static("from-client"),
        );
        map
    }

    #[tokio::test]
    async fn propagates_an_inbound_request_id() {
        let plugin = RequestIdTransformPlugin;
        let inbound = headers();
        let mut outbound = http::HeaderMap::new();
        plugin
            .transform_request(&context(), &serde_json::json!({}), &inbound, &mut outbound)
            .await
            .expect("propagated");
        assert_eq!(
            outbound.get(REQUEST_ID_HEADER).and_then(|value| value.to_str().ok()),
            Some("from-client")
        );
    }

    #[tokio::test]
    async fn propagates_past_dropped_header_rules() {
        // The default passthrough removes every inbound header, so the value
        // the upstream would receive is gone; the plugin still propagates it.
        let plugin = RequestIdTransformPlugin;
        let inbound = headers();
        let mut outbound = http::HeaderMap::new();
        plugin
            .transform_request(&context(), &serde_json::json!({}), &inbound, &mut outbound)
            .await
            .expect("propagated");
        assert_eq!(
            outbound.get(REQUEST_ID_HEADER).and_then(|value| value.to_str().ok()),
            Some("from-client")
        );
    }

    #[tokio::test]
    async fn generates_one_when_absent() {
        let plugin = RequestIdTransformPlugin;
        let mut outbound = http::HeaderMap::new();
        plugin
            .transform_request(&context(), &serde_json::json!({}), &http::HeaderMap::new(), &mut outbound)
            .await
            .expect("generated");
        let value = outbound
            .get(REQUEST_ID_HEADER)
            .and_then(|header| header.to_str().ok())
            .expect("present");
        assert!(value.parse::<uuid::Uuid>().is_ok());
    }

    #[tokio::test]
    async fn response_echoes_the_request_value() {
        let plugin = RequestIdTransformPlugin;
        let inbound = headers();
        let mut outbound = http::HeaderMap::new();
        plugin
            .transform_request(&context(), &serde_json::json!({}), &inbound, &mut outbound)
            .await
            .expect("request");
        let echoed = outbound.get(REQUEST_ID_HEADER).and_then(|value| value.to_str().ok()).map(str::to_owned);
        let mut response = http::HeaderMap::new();
        let context = PluginContext {
            request_id: echoed,
            ..context()
        };
        plugin
            .transform_response(&context, &serde_json::json!({}), http::StatusCode::OK, &mut response)
            .await
            .expect("response");
        assert_eq!(
            response.get(REQUEST_ID_HEADER).and_then(|header| header.to_str().ok()),
            Some("from-client")
        );
    }

    #[test]
    fn empty_value_is_regenerated() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::HeaderName::from_static(REQUEST_ID_HEADER),
            http::HeaderValue::from_static("  "),
        );
        let value = RequestIdTransformPlugin::ensure_request_id(&headers);
        assert!(value.parse::<uuid::Uuid>().is_ok());
    }
}
