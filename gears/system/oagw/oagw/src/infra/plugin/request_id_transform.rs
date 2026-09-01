//! `RequestIdTransformPlugin` — `X-Request-ID` propagation.
//!
//! Config keys: `header` (default `x-request-id`), `mode` (`propagate` |
//! `always`, default `propagate`). In `propagate` mode an inbound identifier
//! is forwarded verbatim; a missing one is generated. In `always` mode a fresh
//! identifier is minted for every request. The response always carries the
//! identifier so callers can correlate.

use crate::domain::error::DomainError;
use crate::domain::gts::TRANSFORM_PLUGIN_REQUEST_ID_INSTANCE;
use crate::domain::plugin::PluginContext;
use async_trait::async_trait;

/// Header the plugin manages.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Mints and propagates request identifiers.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl crate::domain::plugin::TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        TRANSFORM_PLUGIN_REQUEST_ID_INSTANCE
    }

    async fn transform_request(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        parts: &mut http::request::Parts,
    ) -> Result<(), DomainError> {
        let header_name =
            super::config_str(config, "header").unwrap_or_else(|| REQUEST_ID_HEADER.to_owned());
        let name = http::HeaderName::from_bytes(header_name.as_bytes())
            .map_err(|_| DomainError::Validation(format!("invalid header name '{header_name}'")))?;
        let request_id = match parts
            .headers
            .get(&name)
            .and_then(|value| value.to_str().ok())
        {
            Some(existing) if super::config_str(config, "mode").as_deref() != Some("always") => {
                existing.to_owned()
            }
            _ => ctx.request_id().to_owned(),
        };
        if let Ok(value) = http::HeaderValue::from_str(&request_id) {
            parts.headers.insert(name, value);
        }
        Ok(())
    }

    async fn transform_response(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        parts: &mut http::response::Parts,
    ) -> Result<(), DomainError> {
        let header_name =
            super::config_str(config, "header").unwrap_or_else(|| REQUEST_ID_HEADER.to_owned());
        let name = http::HeaderName::from_bytes(header_name.as_bytes())
            .map_err(|_| DomainError::Validation(format!("invalid header name '{header_name}'")))?;
        // The response carries the correlation identifier the request was
        // stamped with, never a freshly minted one, so a caller can correlate
        // the answer with its own request and with the gateway audit line.
        if !parts.headers.contains_key(&name)
            && let Ok(value) = http::HeaderValue::from_str(ctx.request_id())
        {
            parts.headers.insert(name, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::domain::plugin::TransformPlugin;
    use toolkit_security::SecurityContext;

    fn ctx() -> PluginContext {
        PluginContext {
            security_context: SecurityContext::anonymous(),
            upstream_id: uuid::Uuid::nil(),
            host: "vendor.com".to_owned(),
            route_id: None,
            endpoint_host: "api.vendor.com:443".to_owned(),
            request_id: "oagw-test-correlation-id".to_owned(),
        }
    }

    #[tokio::test]
    async fn propagates_an_inbound_request_id() {
        let request = http::Request::builder()
            .header("x-request-id", "abc-123")
            .body(())
            .unwrap();
        let (mut parts, _) = request.into_parts();
        RequestIdTransformPlugin
            .transform_request(&ctx(), &serde_json::json!({}), &mut parts)
            .await
            .unwrap();
        assert_eq!(parts.headers.get("x-request-id").unwrap(), "abc-123");
    }

    #[tokio::test]
    async fn the_response_stamps_the_context_request_id() {
        let response = http::Response::builder().body(()).unwrap();
        let (mut parts, _) = response.into_parts();
        RequestIdTransformPlugin
            .transform_response(&ctx(), &serde_json::json!({}), &mut parts)
            .await
            .unwrap();
        assert_eq!(
            parts.headers.get("x-request-id").unwrap(),
            "oagw-test-correlation-id"
        );
        // An upstream-provided identifier is never overwritten.
        parts.headers.insert(
            http::header::HeaderName::from_static("x-request-id"),
            http::HeaderValue::from_static("upstream-id"),
        );
        RequestIdTransformPlugin
            .transform_response(&ctx(), &serde_json::json!({}), &mut parts)
            .await
            .unwrap();
        assert_eq!(parts.headers.get("x-request-id").unwrap(), "upstream-id");
    }

    #[tokio::test]
    async fn generates_when_absent_and_in_always_mode() {
        let request = http::Request::builder().body(()).unwrap();
        let (mut parts, _) = request.into_parts();
        RequestIdTransformPlugin
            .transform_request(&ctx(), &serde_json::json!({}), &mut parts)
            .await
            .unwrap();
        assert!(parts.headers.get("x-request-id").is_some());

        let request = http::Request::builder()
            .header("x-request-id", "abc-123")
            .body(())
            .unwrap();
        let (mut parts, _) = request.into_parts();
        RequestIdTransformPlugin
            .transform_request(&ctx(), &serde_json::json!({"mode": "always"}), &mut parts)
            .await
            .unwrap();
        assert_ne!(parts.headers.get("x-request-id").unwrap(), "abc-123");
    }

    #[tokio::test]
    async fn honours_a_custom_header_name() {
        let request = http::Request::builder().body(()).unwrap();
        let (mut parts, _) = request.into_parts();
        RequestIdTransformPlugin
            .transform_request(
                &ctx(),
                &serde_json::json!({"header": "x-trace"}),
                &mut parts,
            )
            .await
            .unwrap();
        assert!(parts.headers.get("x-trace").is_some());
    }
}
