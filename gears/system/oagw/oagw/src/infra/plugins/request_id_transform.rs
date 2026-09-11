//! `request_id` transform plugin — `X-Request-ID` propagation (`PRD.md`
//! § 5.3).
//!
//! On the request: propagates the caller's `X-Request-ID` when present,
//! generates one when absent. On the response: echoes the resolved value back
//! to the caller.

use async_trait::async_trait;

use crate::domain::gts_helpers::TRANSFORM_REQUEST_ID;
use crate::domain::plugin::{PluginResult, RequestContext, ResponseContext, TransformPlugin};

/// Header carrying the correlation identifier.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Reads or mints the correlation identifier for the request.
fn request_id(ctx: &RequestContext) -> String {
    if let Some(existing) = ctx
        .headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| ctx.request_id.clone())
    {
        return existing;
    }
    if ctx
        .config
        .get("generate")
        .is_some_and(|value| value == &serde_json::Value::Bool(false))
    {
        return String::new();
    }
    uuid::Uuid::new_v4().to_string()
}

/// Propagates `X-Request-ID` on both legs.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        TRANSFORM_REQUEST_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let id = request_id(ctx);
        if id.is_empty() {
            return Ok(());
        }
        if let Ok(value) = http::HeaderValue::from_str(&id) {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> PluginResult<()> {
        if let Some(id) = &ctx.request_id
            && let Ok(value) = http::HeaderValue::from_str(id)
        {
            ctx.headers.insert(REQUEST_ID_HEADER, value.clone());
        }
        Ok(())
    }

    async fn transform_error(
        &self,
        ctx: &mut crate::domain::plugin::ErrorContext,
    ) -> PluginResult<()> {
        if let Some(id) = &ctx.request_id
            && let Ok(value) = http::HeaderValue::from_str(id)
        {
            ctx.headers.insert(REQUEST_ID_HEADER, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(config: serde_json::Value, headers: &[(&str, &str)]) -> RequestContext {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                http::HeaderName::try_from(*name).expect("name"),
                http::HeaderValue::try_from(*value).expect("value"),
            );
        }
        RequestContext {
            method: http::Method::GET,
            path: "/".to_owned(),
            query: String::new(),
            headers: map,
            body: bytes::Bytes::new(),
            request_id: None,
            config,
            runtime: crate::domain::plugin::test_runtime(),
        }
    }

    #[tokio::test]
    async fn a_supplied_request_id_is_propagated() {
        let plugin = RequestIdTransformPlugin;
        let mut context = ctx(serde_json::json!({}), &[("X-Request-ID", "caller-123")]);
        plugin.transform_request(&mut context).await.expect("ok");
        assert_eq!(context.headers.get("x-request-id").unwrap(), "caller-123");
    }

    #[tokio::test]
    async fn a_missing_request_id_is_minted() {
        let plugin = RequestIdTransformPlugin;
        let mut context = ctx(serde_json::json!({}), &[]);
        plugin.transform_request(&mut context).await.expect("ok");
        let minted = context.headers.get("x-request-id").expect("header");
        assert!(uuid::Uuid::parse_str(minted.to_str().unwrap()).is_ok());
    }

    #[tokio::test]
    async fn the_response_echoes_the_identifier() {
        let plugin = RequestIdTransformPlugin;
        let mut context = crate::domain::plugin::test_response(serde_json::json!({}));
        context.request_id = Some("caller-123".to_owned());
        plugin.transform_response(&mut context).await.expect("ok");
        assert_eq!(context.headers.get("x-request-id").unwrap(), "caller-123");
    }

    #[test]
    fn the_id_is_the_documented_gts_id() {
        assert_eq!(RequestIdTransformPlugin.id(), TRANSFORM_REQUEST_ID);
        assert_eq!(RequestIdTransformPlugin.plugin_type(), "transform");
    }
}
