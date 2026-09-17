//! Built-in transform plugins and the `TransformPlugin` trait
//! ([ADR-0002](../../../../docs/ADR/0002-plugin-system.md)).
//!
//! A transform mutates the request, the response or the error the proxy is
//! about to return. The only built-in transform is
//! [`RequestIdTransformPlugin`], which propagates an inbound `X-Request-ID` or
//! generates one, so the id is the same on the upstream request, the response
//! and the error.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use uuid::Uuid;

use crate::domain::error::OagwError;

use super::{ErrorView, PluginType, RequestContext, UpstreamResponseView};

/// The header the request id travels in.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// Scratch-space key the request id is published under, so the response and
/// error phases reuse the id the request phase chose.
pub const REQUEST_ID_ATTRIBUTE: &str = "request_id";

/// `X-Request-ID` propagation: generates the id when the request carries none
/// and stamps it on the upstream request, the response and the error.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

/// The `TransformPlugin` trait: request/response/error mutation, before and
/// after the upstream call.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The GTS identifier the plugin is registered under.
    fn id(&self) -> &str;

    /// The kind of the plugin.
    fn plugin_type(&self) -> PluginType;

    /// Mutates the request before it is proxied.
    ///
    /// # Errors
    /// Whatever the plugin cannot recover from.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;

    /// Mutates the upstream's response.
    ///
    /// # Errors
    /// Whatever the plugin cannot recover from.
    async fn transform_response(
        &self,
        ctx: &mut RequestContext,
        response: &mut UpstreamResponseView,
    ) -> Result<(), OagwError>;

    /// Mutates the error the proxy is about to return.
    ///
    /// # Errors
    /// Whatever the plugin cannot recover from.
    async fn transform_error(
        &self,
        ctx: &mut RequestContext,
        error: &mut ErrorView,
    ) -> Result<(), OagwError>;
}

/// The request id the chain settled on: the inbound one when it carried a
/// non-blank value, a freshly generated one otherwise.
fn request_id_of(ctx: &RequestContext) -> String {
    match ctx
        .request_headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        Some(existing) if !existing.is_empty() => existing.to_owned(),
        _ => Uuid::now_v7().to_string(),
    }
}

fn header_value(request_id: &str) -> Result<HeaderValue, OagwError> {
    HeaderValue::from_str(request_id).map_err(|_| OagwError::Validation {
        message: "the request id is not a valid header value".to_owned(),
    })
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        super::registry::REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Transform
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let request_id = request_id_of(ctx);
        ctx.set_attribute(REQUEST_ID_ATTRIBUTE, request_id.clone());
        ctx.request_headers.insert(
            HeaderName::from_static(REQUEST_ID_HEADER),
            header_value(&request_id)?,
        );
        Ok(())
    }

    async fn transform_response(
        &self,
        ctx: &mut RequestContext,
        response: &mut UpstreamResponseView,
    ) -> Result<(), OagwError> {
        let request_id = ctx
            .attribute(REQUEST_ID_ATTRIBUTE)
            .map_or_else(|| request_id_of(ctx), std::borrow::ToOwned::to_owned);
        response.headers.insert(
            HeaderName::from_static(REQUEST_ID_HEADER),
            header_value(&request_id)?,
        );
        Ok(())
    }

    async fn transform_error(
        &self,
        ctx: &mut RequestContext,
        error: &mut ErrorView,
    ) -> Result<(), OagwError> {
        let request_id = ctx
            .attribute(REQUEST_ID_ATTRIBUTE)
            .map_or_else(|| request_id_of(ctx), std::borrow::ToOwned::to_owned);
        error.headers.insert(
            HeaderName::from_static(REQUEST_ID_HEADER),
            header_value(&request_id)?,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use http::{HeaderName, HeaderValue, StatusCode};
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::{
        REQUEST_ID_ATTRIBUTE, REQUEST_ID_HEADER, RequestIdTransformPlugin, TransformPlugin as _,
    };
    use crate::infra::plugins::{ErrorView, PluginType, RequestContext, UpstreamResponseView};

    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::now_v7())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("valid security context")
    }

    fn context(request_id: Option<&str>) -> RequestContext {
        let mut context = RequestContext::new(security(), Uuid::new_v4(), "/v1/chat");
        if let Some(request_id) = request_id {
            context.request_headers.insert(
                HeaderName::from_static(REQUEST_ID_HEADER),
                HeaderValue::from_str(request_id).unwrap(),
            );
        }
        context
    }

    #[tokio::test]
    async fn a_missing_request_id_is_generated() {
        let plugin = RequestIdTransformPlugin;
        let mut context = context(None);

        plugin.transform_request(&mut context).await.unwrap();

        let generated = context.attribute(REQUEST_ID_ATTRIBUTE).expect("request id");
        assert!(
            Uuid::parse_str(generated).is_ok(),
            "generated id: {generated}"
        );
        assert_eq!(
            context
                .request_headers
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(generated)
        );
    }

    #[tokio::test]
    async fn an_inbound_request_id_is_propagated() {
        let plugin = RequestIdTransformPlugin;
        let mut context = context(Some("trace-42"));

        plugin.transform_request(&mut context).await.unwrap();

        assert_eq!(context.attribute(REQUEST_ID_ATTRIBUTE), Some("trace-42"));
        assert_eq!(
            context
                .request_headers
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("trace-42")
        );
    }

    #[tokio::test]
    async fn the_response_carries_the_request_id() {
        let plugin = RequestIdTransformPlugin;
        let mut context = context(Some("trace-42"));
        plugin.transform_request(&mut context).await.unwrap();
        let mut response = UpstreamResponseView::new(StatusCode::OK);

        plugin
            .transform_response(&mut context, &mut response)
            .await
            .unwrap();

        assert_eq!(
            response
                .headers
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("trace-42")
        );
    }

    #[tokio::test]
    async fn the_error_carries_the_request_id() {
        let plugin = RequestIdTransformPlugin;
        let mut context = context(None);
        plugin.transform_request(&mut context).await.unwrap();
        let mut error = ErrorView {
            status: StatusCode::BAD_GATEWAY,
            error_code: "UPSTREAM".to_owned(),
            message: "upstream error".to_owned(),
            headers: http::HeaderMap::new(),
        };

        plugin
            .transform_error(&mut context, &mut error)
            .await
            .unwrap();

        let stamped = context.attribute(REQUEST_ID_ATTRIBUTE).expect("request id");
        assert_eq!(
            error
                .headers
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(stamped)
        );
    }

    #[tokio::test]
    async fn a_response_without_a_request_phase_generates_its_own_id() {
        let plugin = RequestIdTransformPlugin;
        let mut context = context(None);
        let mut response = UpstreamResponseView::new(StatusCode::OK);

        plugin
            .transform_response(&mut context, &mut response)
            .await
            .unwrap();

        let stamped = response
            .headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .expect("response id");
        assert!(Uuid::parse_str(stamped).is_ok());
    }

    #[test]
    fn the_plugin_is_a_transform() {
        let plugin = RequestIdTransformPlugin;
        assert_eq!(plugin.plugin_type(), PluginType::Transform);
        assert_eq!(
            plugin.id(),
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        );
    }
}
