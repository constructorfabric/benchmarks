//! Built-in transform plugins.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::transform_plugin_ids;
use crate::domain::plugin::{ErrorContext, RequestContext, ResponseContext, TransformPlugin};

/// Header carrying the propagated request identifier.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Propagates `X-Request-ID` between the caller and the upstream.
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        transform_plugin_ids::REQUEST_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        if ctx.header(REQUEST_ID_HEADER).is_none() {
            ctx.set_header(REQUEST_ID_HEADER, &Uuid::new_v4().to_string());
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError> {
        if ctx.header(REQUEST_ID_HEADER).is_none() {
            ctx.set_header(REQUEST_ID_HEADER, &Uuid::new_v4().to_string());
        }
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError> {
        if ctx
            .attributes
            .get(REQUEST_ID_HEADER)
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            ctx.attributes.insert(
                REQUEST_ID_HEADER.to_owned(),
                serde_json::Value::String(Uuid::new_v4().to_string()),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn request(config: serde_json::Value) -> RequestContext {
        RequestContext {
            tenant_id: Uuid::nil(),
            caller_tenant_id: Uuid::nil(),
            subject_id: String::new(),
            method: http::Method::GET,
            path: "/".to_owned(),
            query: None,
            headers: http::HeaderMap::default(),
            inbound_headers: http::HeaderMap::default(),
            client_ip: None,
            security: None,
            config,
            attributes: std::collections::BTreeMap::default(),
        }
    }

    #[tokio::test]
    async fn injects_request_id_when_absent() {
        let mut ctx = request(serde_json::Value::Null);
        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .unwrap_or_default();
        assert!(ctx.header(REQUEST_ID_HEADER).is_some());
    }
}
