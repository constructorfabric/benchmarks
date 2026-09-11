//! Built-in request-id transform plugin.

use async_trait::async_trait;

use crate::domain::plugin::{
    ErrorContext, PluginError, PluginPhase, RequestContext, ResponseContext, TransformPlugin,
};
use crate::infra::proxy::headers::REQUEST_ID;

/// Propagates an inbound `X-Request-ID` and generates one when absent.
pub struct RequestIdTransformPlugin;

/// Generate a request id.
#[must_use]
pub fn generate() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        "request_id"
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let existing = ctx
            .headers
            .get(REQUEST_ID)
            .and_then(|v| v.to_str().ok())
            .map_or_else(generate, str::to_owned);
        if let Ok(value) = http::HeaderValue::from_str(&existing) {
            let name = http::HeaderName::from_static(REQUEST_ID);
            ctx.headers.insert(name, value);
        }
        ctx.record(self.id(), PluginPhase::Request);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        ctx.record(self.id(), PluginPhase::Response);
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        ctx.record(self.id(), PluginPhase::Error);
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use http::{HeaderMap, HeaderValue};
    use serde_json::json;
    use std::sync::Arc;

    use super::*;
    use crate::domain::plugin::gts_helpers;
    use crate::infra::plugin::test_support::{recorded, request_context, response_context};

    fn id_of(ctx: &RequestContext) -> String {
        ctx.headers
            .get(REQUEST_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn generated_ids_are_uuids() {
        let first = generate();
        let second = generate();
        uuid::Uuid::parse_str(&first).expect("the id is a uuid");
        assert_ne!(first, second, "every generated id is fresh");
    }

    #[tokio::test]
    async fn a_missing_id_is_generated() {
        let mut ctx = request_context("local");
        ctx.config = json!({});

        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .expect("the transform never fails");
        uuid::Uuid::parse_str(&id_of(&ctx)).expect("the id is a uuid");
        assert_eq!(recorded(&ctx), vec!["request_id:Request".to_owned()]);
    }

    #[tokio::test]
    async fn an_inbound_id_is_propagated_verbatim() {
        let mut ctx = request_context("local");
        ctx.headers
            .insert(REQUEST_ID, HeaderValue::from_static("caller-set-42"));

        RequestIdTransformPlugin
            .transform_request(&mut ctx)
            .await
            .expect("the transform never fails");
        assert_eq!(id_of(&ctx), "caller-set-42");
    }

    #[tokio::test]
    async fn the_response_and_error_phases_record_without_touching_headers() {
        let mut response = response_context("local");
        response
            .headers
            .insert("x-a", HeaderValue::from_static("1"));
        RequestIdTransformPlugin
            .transform_response(&mut response)
            .await
            .expect("the response phase never fails");
        assert_eq!(response.headers.get("x-a").unwrap(), "1");

        let mut error = crate::domain::plugin::ErrorContext {
            status: http::StatusCode::BAD_GATEWAY,
            headers: HeaderMap::new(),
            alias: "local".to_owned(),
            error: crate::domain::error::DomainError::DownstreamError("boom".to_owned()),
            config: json!({}),
            trace: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        RequestIdTransformPlugin
            .transform_error(&mut error)
            .await
            .expect("the error phase never fails");
        assert!(error.headers.is_empty());

        assert_eq!(
            recorded(&request_context("local")),
            Vec::<String>::new(),
            "the request context is untouched by the other phases"
        );
    }

    #[test]
    fn the_gts_identifier_follows_the_transform_family() {
        assert_eq!(RequestIdTransformPlugin.id(), "request_id");
        assert_eq!(
            RequestIdTransformPlugin.gts_id(),
            gts_helpers::REQUEST_ID_TRANSFORM
        );
    }
}
