//! `request_id` transform plugin — `X-Request-ID` propagation.
//!
//! Propagates an inbound correlation id to the upstream, minting one when the
//! caller did not supply it, and echoes it back on the response so a client
//! can correlate without reading gateway logs.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::PluginError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{ErrorContext, RequestContext, ResponseContext, TransformPlugin};

/// Header carrying the correlation id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// `request_id` built-in transform plugin.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

fn header_name(config: &serde_json::Map<String, serde_json::Value>) -> String {
    config
        .get("header_name")
        .or_else(|| config.get("header"))
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| REQUEST_ID_HEADER.to_owned())
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        gts::REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let name = header_name(&ctx.config);
        if ctx.headers.contains_key(name.as_str()) {
            return Ok(());
        }
        let minted = Uuid::new_v4().to_string();
        ctx.set_header(&name, &minted)
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        let name = header_name(&ctx.config);
        if ctx.headers.contains_key(name.as_str()) {
            return Ok(());
        }
        let Some(request_id) = ctx.request_id.clone() else {
            return Ok(());
        };
        let header: http::HeaderName = name
            .parse()
            .map_err(|_| PluginError::Config(format!("invalid header name '{name}'")))?;
        let value = http::HeaderValue::from_str(&request_id).map_err(|_| {
            PluginError::Config("request id is not a legal header value".to_owned())
        })?;
        ctx.headers.insert(header, value);
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{REQUEST_ID_HEADER, RequestIdTransformPlugin};
    use crate::domain::plugin::{RequestContext, ResponseContext, TransformPlugin};
    use http::{HeaderMap, HeaderValue, Method, StatusCode};
    use serde_json::{Value, json};
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn request_ctx(config: Value, existing: Option<&str>) -> RequestContext {
        let mut headers = HeaderMap::new();
        if let Some(value) = existing {
            headers.insert(
                REQUEST_ID_HEADER,
                HeaderValue::from_str(value).expect("value"),
            );
        }
        RequestContext {
            method: Method::GET,
            path: "/v1/models".to_owned(),
            query: Vec::new(),
            headers,
            config: config.as_object().cloned().unwrap_or_default(),
            security_context: SecurityContext::anonymous(),
            alias: "api.openai.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            route_id: None,
        }
    }

    #[tokio::test]
    async fn mints_a_request_id_when_the_caller_sent_none() {
        let mut ctx = request_ctx(json!({}), None);
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .expect("transformed");
        let minted = ctx.headers[REQUEST_ID_HEADER].to_str().expect("ascii");
        assert!(Uuid::parse_str(minted).is_ok(), "minted a uuid: {minted}");
    }

    #[tokio::test]
    async fn propagates_an_existing_request_id_unchanged() {
        let mut ctx = request_ctx(json!({}), Some("req_abc123"));
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .expect("transformed");
        assert_eq!(ctx.headers[REQUEST_ID_HEADER], "req_abc123");
    }

    #[tokio::test]
    async fn honours_a_configured_header_name() {
        let mut ctx = request_ctx(json!({ "header_name": "X-Correlation-Id" }), None);
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .expect("transformed");
        assert!(ctx.headers.contains_key("x-correlation-id"));
        assert!(!ctx.headers.contains_key(REQUEST_ID_HEADER));
    }

    #[tokio::test]
    async fn echoes_the_request_id_onto_the_response() {
        let mut ctx = ResponseContext {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            config: serde_json::Map::new(),
            request_id: Some("req_abc123".to_owned()),
        };
        RequestIdTransformPlugin
            .transform_response(&mut ctx)
            .await
            .expect("transformed");
        assert_eq!(ctx.headers[REQUEST_ID_HEADER], "req_abc123");
    }

    #[tokio::test]
    async fn does_not_overwrite_an_upstream_supplied_request_id() {
        let mut headers = HeaderMap::new();
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("upstream-id"));
        let mut ctx = ResponseContext {
            status: StatusCode::OK,
            headers,
            config: serde_json::Map::new(),
            request_id: Some("req_abc123".to_owned()),
        };
        RequestIdTransformPlugin
            .transform_response(&mut ctx)
            .await
            .expect("transformed");
        assert_eq!(ctx.headers[REQUEST_ID_HEADER], "upstream-id");
    }
}
