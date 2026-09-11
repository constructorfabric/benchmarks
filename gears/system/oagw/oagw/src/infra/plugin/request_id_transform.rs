//! The `request_id` transform plugin: `X-Request-ID` propagation.
//!
//! The upstream receives the caller's id when it sent one and a freshly minted
//! id otherwise; the response carries the same id back so a client can
//! correlate a round trip end to end.

use async_trait::async_trait;

use crate::domain::gts_helpers::BUILTIN_TRANSFORM_REQUEST_ID;
use crate::domain::plugin::{
    PluginError, RequestContext, ResponseContext, TransformPlugin,
};

/// The header the plugin propagates.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Propagates `X-Request-ID` across the gateway.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestIdTransform;

#[async_trait]
impl TransformPlugin for RequestIdTransform {
    fn id(&self) -> &str {
        BUILTIN_TRANSFORM_REQUEST_ID
    }

    fn plugin_type(&self) -> &str {
        crate::domain::gts_helpers::TRANSFORM_PLUGIN_TYPE
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let request_id = match ctx.header(REQUEST_ID_HEADER) {
            Some(existing) => existing.to_string(),
            None => {
                let minted = uuid::Uuid::new_v4().to_string();
                ctx.set_header(REQUEST_ID_HEADER, minted.clone());
                minted
            }
        };
        ctx.request_id = Some(request_id);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        let request_id = ctx
            .attributes
            .get("oagw.request_id")
            .cloned()
            .unwrap_or_default();
        if !request_id.is_empty() && ctx.header(REQUEST_ID_HEADER).is_none() {
            ctx.set_header(REQUEST_ID_HEADER, request_id);
        }
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut crate::domain::plugin::ErrorContext) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_existing_id_is_propagated_unchanged() {
        let mut ctx = RequestContext::default();
        ctx.set_header("X-Request-ID", "caller-id");
        RequestIdTransform.transform_request(&mut ctx).await.unwrap();
        assert_eq!(ctx.request_id.as_deref(), Some("caller-id"));
        assert_eq!(ctx.header("x-request-id"), Some("caller-id"));
    }

    #[tokio::test]
    async fn a_missing_id_is_minted() {
        let mut ctx = RequestContext::default();
        RequestIdTransform.transform_request(&mut ctx).await.unwrap();
        let minted = ctx.request_id.clone().expect("an id was minted");
        assert!(uuid::Uuid::parse_str(&minted).is_ok());
        assert_eq!(ctx.header("x-request-id"), Some(minted.as_str()));
    }

    #[tokio::test]
    async fn the_response_carries_the_id_back() {
        let mut response = ResponseContext::default();
        response
            .attributes
            .insert("oagw.request_id".to_string(), "caller-id".to_string());
        RequestIdTransform.transform_response(&mut response).await.unwrap();
        assert_eq!(response.header("x-request-id"), Some("caller-id"));
    }

    #[test]
    fn the_plugin_id_is_the_builtin_identifier() {
        assert_eq!(RequestIdTransform.id(), BUILTIN_TRANSFORM_REQUEST_ID);
    }
}
